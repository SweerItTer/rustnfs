#![cfg_attr(windows, windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]

// The GUI owns only its own child process. It never installs a service,
// elevates privileges, or changes Windows Firewall automatically.

#[cfg(not(windows))]
fn main() {
    eprintln!("RustNFS 的图形界面需要 Windows。此平台请运行 rustnfsd --config rustnfs.json。");
}

#[cfg(windows)]
fn main() {
    if let Err(message) = native::run() {
        native::show_startup_error(&message);
    }
}

#[cfg(windows)]
mod native {
    use rustnfs::config::{Config, ExportConfig};
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Ipv4Addr, UdpSocket};
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::ptr::{null, null_mut};
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::Com::*;
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::LibraryLoader::*;
    use windows_sys::Win32::System::Memory::*;
    use windows_sys::Win32::UI::Controls::*;
    use windows_sys::Win32::UI::HiDpi::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows_sys::Win32::UI::Shell::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    const TIMER: usize = 1;
    const ID_BIND: u16 = 101;
    const ID_CLIENTS: u16 = 102;
    const ID_HOST: u16 = 103;
    const ID_PORTMAP: u16 = 104;
    const ID_MOUNT: u16 = 105;
    const ID_NFS: u16 = 106;
    const ID_RESET_PORTS: u16 = 107;
    const ID_SAVE: u16 = 108;
    const ID_LIST: u16 = 110;
    const ID_PATH: u16 = 111;
    const ID_BROWSE: u16 = 112;
    const ID_NAME: u16 = 113;
    const ID_READONLY: u16 = 114;
    const ID_ADD: u16 = 115;
    const ID_UPDATE: u16 = 116;
    const ID_REMOVE: u16 = 117;
    const ID_CLEAR: u16 = 118;
    const ID_START: u16 = 120;
    const ID_STOP: u16 = 121;
    const ID_HELP: u16 = 122;
    const ID_TCP: u16 = 123;
    const ID_UDP: u16 = 124;
    const BG: u32 = 0x00f8f6f3;
    const TEXT: u32 = 0x003a3028;
    const MUTED: u32 = 0x007a7166;
    const SS_LEFT: u32 = 0;
    const SS_RIGHT: u32 = 2;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    unsafe fn set_text(hwnd: HWND, text: &str) {
        SetWindowTextW(hwnd, wide(text).as_ptr());
    }

    unsafe fn get_text(hwnd: HWND) -> String {
        let count = GetWindowTextLengthW(hwnd).clamp(0, 32767) as usize;
        let mut buffer = vec![0u16; count + 1];
        let read = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..read.max(0) as usize])
    }

    fn current_ipv4() -> String {
        // UDP connect only chooses a route; it sends no datagram.
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|socket| {
                socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
                socket.local_addr()
            })
            .map(|address| address.ip().to_string())
            .unwrap_or_default()
    }

    unsafe extern "system" fn font_found(_: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, result: isize) -> i32 {
        *(result as *mut bool) = true;
        0
    }

    unsafe fn choose_font(candidates: &[&str]) -> String {
        let dc = GetDC(null_mut());
        for name in candidates {
            let mut query: LOGFONTW = std::mem::zeroed();
            query.lfCharSet = DEFAULT_CHARSET;
            for (target, source) in query.lfFaceName.iter_mut().zip(name.encode_utf16()) {
                *target = source;
            }
            let mut exists = false;
            EnumFontFamiliesExW(dc, &query, Some(font_found), &mut exists as *mut bool as isize, 0);
            if exists {
                ReleaseDC(null_mut(), dc);
                return (*name).to_owned();
            }
        }
        ReleaseDC(null_mut(), dc);
        candidates.last().copied().unwrap_or("Segoe UI").to_owned()
    }

    pub fn show_startup_error(message: &str) {
        unsafe {
            MessageBoxW(
                null_mut(),
                wide(message).as_ptr(),
                wide("RustNFS 无法启动").as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    #[derive(Clone, Copy)]
    struct Controls {
        title: HWND,
        subtitle: HWND,
        status: HWND,
        network_group: HWND,
        bind_label: HWND,
        bind: HWND,
        host_label: HWND,
        host: HWND,
        clients_label: HWND,
        clients: HWND,
        clients_hint: HWND,
        portmap_label: HWND,
        portmap: HWND,
        mount_label: HWND,
        mount: HWND,
        nfs_label: HWND,
        nfs: HWND,
        reset_ports: HWND,
        save: HWND,
        shares_group: HWND,
        list: HWND,
        path_label: HWND,
        path: HWND,
        browse: HWND,
        name_label: HWND,
        name: HWND,
        readonly: HWND,
        add: HWND,
        update: HWND,
        remove: HWND,
        clear: HWND,
        shares_hint: HWND,
        command_group: HWND,
        command: HWND,
        copy_tcp: HWND,
        copy_udp: HWND,
        start: HWND,
        stop: HWND,
        help: HWND,
        service_hint: HWND,
        log_label: HWND,
        log: HWND,
    }

    impl Default for Controls {
        fn default() -> Self {
            // All fields are initially null HWND values, not live controls.
            unsafe { std::mem::zeroed() }
        }
    }

    enum Event {
        Line(String),
        Ready,
    }

    struct App {
        hwnd: HWND,
        instance: HINSTANCE,
        controls: Controls,
        dpi: u32,
        font: HFONT,
        title_font: HFONT,
        mono_font: HFONT,
        background: HBRUSH,
        config: Config,
        config_load_failed: bool,
        config_path: PathBuf,
        directory: PathBuf,
        selected: Option<usize>,
        child: Option<Child>,
        events: Receiver<Event>,
        sender: SyncSender<Event>,
        lines: VecDeque<String>,
        ready: bool,
        stop_deadline: Option<Instant>,
        closing: bool,
        updating: bool,
    }

    impl App {
        fn new(instance: HINSTANCE, directory: PathBuf, background: HBRUSH) -> Self {
            let config_path = directory.join("rustnfs.json");
            let (sender, events) = mpsc::sync_channel(512);
            let (config, note, config_load_failed) = if config_path.exists() {
                match Config::load(&config_path) {
                    Ok(config) => (config, Some("已读取 rustnfs.json。".to_owned()), false),
                    Err(error) => (
                        Config::default(),
                        Some(format!("配置读取失败，尚未覆盖原文件：{error}")),
                        true,
                    ),
                }
            } else {
                (Config::default(), Some("请添加共享目录，并填写开发板 IP 或网段。".to_owned()), false)
            };
            let mut lines = VecDeque::new();
            if let Some(note) = note {
                lines.push_back(note);
            }
            Self {
                hwnd: null_mut(),
                instance,
                controls: Controls::default(),
                dpi: 96,
                font: null_mut(),
                title_font: null_mut(),
                mono_font: null_mut(),
                background,
                config,
                config_load_failed,
                config_path,
                directory,
                selected: None,
                child: None,
                events,
                sender,
                lines,
                ready: false,
                stop_deadline: None,
                closing: false,
                updating: false,
            }
        }

        unsafe fn control(&self, class: &str, text: &str, id: u16, style: u32) -> HWND {
            let result = CreateWindowExW(
                if class == "EDIT" || class == "LISTBOX" { WS_EX_CLIENTEDGE } else { 0 },
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                0, 0, 1, 1,
                self.hwnd,
                id as usize as HMENU,
                self.instance,
                null(),
            );
            if !result.is_null() {
                SendMessageW(result, WM_SETFONT, self.font as usize, 1);
            }
            result
        }

        unsafe fn fonts(&mut self) {
            for font in [self.font, self.title_font, self.mono_font] {
                if !font.is_null() { DeleteObject(font); }
            }
            let create = |height: i32, weight: u32, face: &str| {
                CreateFontW(
                    -(height * self.dpi as i32 / 96), 0, 0, 0, weight as i32,
                    0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32,
                    CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32,
                    DEFAULT_PITCH as u32, wide(face).as_ptr(),
                )
            };
            let face = choose_font(&["Microsoft YaHei UI", "Microsoft YaHei", "WenQuanYi Micro Hei", "Segoe UI"]);
            let mono = choose_font(&["WenQuanYi Micro Hei Mono", "Consolas", "Courier New"]);
            self.font = create(14, FW_NORMAL, &face);
            self.title_font = create(25, FW_SEMIBOLD, &face);
            self.mono_font = create(13, FW_NORMAL, &mono);
        }

        unsafe fn init_controls(&mut self) -> Result<(), String> {
            self.dpi = GetDpiForWindow(self.hwnd).max(96);
            self.fonts();
            let c = |class, text, id, style| self.control(class, text, id, style);
            self.controls = Controls {
                title: c("STATIC", "RustNFS", 0, SS_LEFT),
                subtitle: c("STATIC", "将 Windows 文件夹共享给开发板 · NFSv3 · TCP / UDP", 0, SS_LEFT),
                status: c("STATIC", "● 已停止", 0, SS_RIGHT),
                network_group: c("BUTTON", "连接设置", 0, BS_GROUPBOX as u32),
                bind_label: c("STATIC", "监听 IPv4", 0, SS_LEFT),
                bind: c("EDIT", "", ID_BIND, WS_TABSTOP | ES_AUTOHSCROLL as u32),
                host_label: c("STATIC", "本机 IPv4（用于命令）", 0, SS_LEFT),
                host: c("EDIT", "", ID_HOST, WS_TABSTOP | ES_AUTOHSCROLL as u32),
                clients_label: c("STATIC", "允许客户端 IP / 网段", 0, SS_LEFT),
                clients: c("EDIT", "", ID_CLIENTS, WS_TABSTOP | ES_AUTOHSCROLL as u32),
                clients_hint: c("STATIC", "例如 192.168.1.50/32 只允许该设备；多个地址用逗号分隔。", 0, SS_LEFT),
                portmap_label: c("STATIC", "Portmapper", 0, SS_LEFT),
                portmap: c("EDIT", "", ID_PORTMAP, WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_NUMBER as u32),
                mount_label: c("STATIC", "MOUNT", 0, SS_LEFT),
                mount: c("EDIT", "", ID_MOUNT, WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_NUMBER as u32),
                nfs_label: c("STATIC", "NFS", 0, SS_LEFT),
                nfs: c("EDIT", "", ID_NFS, WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_NUMBER as u32),
                reset_ports: c("BUTTON", "恢复默认端口", ID_RESET_PORTS, WS_TABSTOP | BS_PUSHBUTTON as u32),
                save: c("BUTTON", "保存设置", ID_SAVE, WS_TABSTOP | BS_PUSHBUTTON as u32),
                shares_group: c("BUTTON", "共享目录", 0, BS_GROUPBOX as u32),
                list: c("LISTBOX", "", ID_LIST, WS_TABSTOP | WS_VSCROLL | WS_HSCROLL | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32),
                path_label: c("STATIC", "本地目录", 0, SS_LEFT),
                path: c("EDIT", "", ID_PATH, WS_TABSTOP | ES_AUTOHSCROLL as u32),
                browse: c("BUTTON", "浏览…", ID_BROWSE, WS_TABSTOP | BS_PUSHBUTTON as u32),
                name_label: c("STATIC", "共享名称", 0, SS_LEFT),
                name: c("EDIT", "share", ID_NAME, WS_TABSTOP | ES_AUTOHSCROLL as u32),
                readonly: c("BUTTON", "只读共享", ID_READONLY, WS_TABSTOP | BS_AUTOCHECKBOX as u32),
                add: c("BUTTON", "添加共享", ID_ADD, WS_TABSTOP | BS_PUSHBUTTON as u32),
                update: c("BUTTON", "更新选中", ID_UPDATE, WS_TABSTOP | BS_PUSHBUTTON as u32),
                remove: c("BUTTON", "移除", ID_REMOVE, WS_TABSTOP | BS_PUSHBUTTON as u32),
                clear: c("BUTTON", "清空编辑", ID_CLEAR, WS_TABSTOP | BS_PUSHBUTTON as u32),
                shares_hint: c("STATIC", "填好目录后点击“添加共享”；选中列表条目后可以更新。", 0, SS_LEFT),
                command_group: c("BUTTON", "开发板挂载命令", 0, BS_GROUPBOX as u32),
                command: c("EDIT", "", 0, WS_TABSTOP | ES_READONLY as u32 | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | WS_VSCROLL),
                copy_tcp: c("BUTTON", "复制 TCP", ID_TCP, WS_TABSTOP | BS_PUSHBUTTON as u32),
                copy_udp: c("BUTTON", "复制 UDP", ID_UDP, WS_TABSTOP | BS_PUSHBUTTON as u32),
                start: c("BUTTON", "启动服务", ID_START, WS_TABSTOP | BS_DEFPUSHBUTTON as u32),
                stop: c("BUTTON", "停止服务", ID_STOP, WS_TABSTOP | BS_PUSHBUTTON as u32),
                help: c("BUTTON", "使用说明", ID_HELP, WS_TABSTOP | BS_PUSHBUTTON as u32),
                service_hint: c("STATIC", "关闭窗口会停止本程序启动的共享服务。", 0, SS_LEFT),
                log_label: c("STATIC", "运行日志", 0, SS_LEFT),
                log: c("EDIT", "", 0, WS_TABSTOP | ES_READONLY as u32 | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | WS_VSCROLL),
            };
            if self.controls.log.is_null() || self.controls.start.is_null() {
                return Err("创建 Windows 界面控件失败。".to_owned());
            }
            SendMessageW(self.controls.title, WM_SETFONT, self.title_font as usize, 1);
            SendMessageW(self.controls.command, WM_SETFONT, self.mono_font as usize, 1);
            SendMessageW(self.controls.log, WM_SETFONT, self.mono_font as usize, 1);
            for (handle, maximum) in [(self.controls.name, 64), (self.controls.path, 16000),
                (self.controls.bind, 64), (self.controls.host, 64), (self.controls.clients, 4096),
                (self.controls.portmap, 5), (self.controls.mount, 5), (self.controls.nfs, 5)] {
                SendMessageW(handle, EM_SETLIMITTEXT, maximum, 0);
            }
            set_text(self.controls.bind, &self.config.bind_address);
            set_text(self.controls.clients, &self.config.allowed_clients.join(", "));
            self.set_ports(self.config.portmap_port, self.config.mount_port, self.config.nfs_port);
            let host = if self.config.bind_address != "0.0.0.0" {
                self.config.bind_address.clone()
            } else { current_ipv4() };
            set_text(self.controls.host, &host);
            self.refresh_list();
            self.select(if self.config.exports.is_empty() { None } else { Some(0) });
            self.render_log();
            self.update_command();
            self.lock_controls(false);
            self.layout();
            SetTimer(self.hwnd, TIMER, 100, None);
            Ok(())
        }

        fn px(&self, value: i32) -> i32 { value * self.dpi as i32 / 96 }

        unsafe fn layout(&self) {
            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut rect);
            let width = rect.right * 96 / self.dpi as i32;
            let height = rect.bottom * 96 / self.dpi as i32;
            let c = self.controls;
            let place = |handle: HWND, x: i32, y: i32, w: i32, h: i32| {
                MoveWindow(handle, self.px(x), self.px(y), self.px(w.max(1)), self.px(h.max(1)), 1);
            };
            place(c.title, 24, 17, 300, 34);
            place(c.subtitle, 24, 55, width - 48, 21);
            place(c.status, width - 280, 25, 254, 25);
            place(c.network_group, 20, 86, width - 40, 162);
            place(c.bind_label, 36, 115, 92, 22);
            place(c.bind, 132, 108, 190, 29);
            place(c.host_label, 348, 115, 174, 22);
            place(c.host, 530, 108, width - 566, 29);
            place(c.clients_label, 36, 154, 190, 22);
            place(c.clients, 232, 147, width - 268, 29);
            place(c.clients_hint, 36, 181, width - 72, 21);
            place(c.portmap_label, 36, 218, 88, 22);
            place(c.portmap, 128, 211, 76, 29);
            place(c.mount_label, 226, 218, 60, 22);
            place(c.mount, 292, 211, 76, 29);
            place(c.nfs_label, 390, 218, 36, 22);
            place(c.nfs, 432, 211, 76, 29);
            place(c.reset_ports, 530, 211, 130, 29);
            place(c.save, width - 152, 211, 116, 29);
            place(c.shares_group, 20, 258, width - 40, 220);
            place(c.list, 36, 283, width - 72, 62);
            place(c.path_label, 36, 362, 76, 22);
            place(c.path, 114, 355, width - 242, 29);
            place(c.browse, width - 116, 355, 80, 29);
            place(c.name_label, 36, 400, 76, 22);
            place(c.name, 114, 393, 190, 29);
            place(c.readonly, 326, 395, 126, 26);
            place(c.add, 36, 435, 100, 29);
            place(c.update, 146, 435, 100, 29);
            place(c.remove, 256, 435, 74, 29);
            place(c.clear, 340, 435, 94, 29);
            place(c.shares_hint, 454, 438, width - 490, 32);
            place(c.command_group, 20, 488, width - 40, 100);
            place(c.command, 36, 512, width - 204, 61);
            place(c.copy_tcp, width - 152, 512, 116, 28);
            place(c.copy_udp, width - 152, 546, 116, 28);
            place(c.start, 24, 603, 112, 34);
            place(c.stop, 146, 603, 112, 34);
            place(c.help, 268, 603, 100, 34);
            place(c.service_hint, 390, 611, width - 414, 24);
            place(c.log_label, 24, 652, width - 48, 21);
            place(c.log, 24, 678, width - 48, height - 698);
        }

        unsafe fn error(&self, message: &str) {
            MessageBoxW(self.hwnd, wide(message).as_ptr(), wide("RustNFS").as_ptr(), MB_OK | MB_ICONERROR);
        }

        fn push_log(&mut self, value: impl Into<String>) {
            let value: String = value.into().chars().take(3000).collect();
            self.lines.push_back(value.replace('\0', "�"));
            while self.lines.len() > 160 { self.lines.pop_front(); }
        }

        unsafe fn log(&mut self, value: impl Into<String>) {
            self.push_log(value);
            self.render_log();
        }

        unsafe fn render_log(&self) {
            let lines: Vec<_> = self.lines.iter().map(String::as_str).collect();
            set_text(self.controls.log, &lines.join("\r\n"));
            SendMessageW(self.controls.log, EM_SETSEL, usize::MAX, -1);
            SendMessageW(self.controls.log, EM_SCROLLCARET, 0, 0);
        }

        unsafe fn refresh_list(&self) {
            SendMessageW(self.controls.list, LB_RESETCONTENT, 0, 0);
            let dc = GetDC(self.controls.list);
            let old_font = SelectObject(dc, self.font);
            let mut maximum = 0;
            for share in &self.config.exports {
                let label = format!("/{}    [{}]    {}", share.name,
                    if share.read_only { "只读" } else { "读写" }, share.path);
                let label = wide(&label);
                SendMessageW(self.controls.list, LB_ADDSTRING, 0, label.as_ptr() as isize);
                let mut size: SIZE = std::mem::zeroed();
                if GetTextExtentPoint32W(dc, label.as_ptr(), label.len() as i32 - 1, &mut size) != 0 {
                    maximum = maximum.max(size.cx + self.px(24));
                }
            }
            SelectObject(dc, old_font);
            ReleaseDC(self.controls.list, dc);
            SendMessageW(self.controls.list, LB_SETHORIZONTALEXTENT, maximum as usize, 0);
        }

        unsafe fn select(&mut self, selected: Option<usize>) {
            self.selected = selected.filter(|index| *index < self.config.exports.len());
            self.updating = true;
            SendMessageW(self.controls.list, LB_SETCURSEL,
                self.selected.unwrap_or(usize::MAX), 0);
            if let Some(index) = self.selected {
                let share = &self.config.exports[index];
                set_text(self.controls.path, &share.path);
                set_text(self.controls.name, &share.name);
                SendMessageW(self.controls.readonly, BM_SETCHECK,
                    if share.read_only { BST_CHECKED } else { BST_UNCHECKED } as usize, 0);
            } else {
                set_text(self.controls.path, "");
                set_text(self.controls.name, "share");
                SendMessageW(self.controls.readonly, BM_SETCHECK, BST_CHECKED as usize, 0);
            }
            self.updating = false;
            self.lock_controls(self.child.is_some());
            self.update_command();
        }

        unsafe fn set_ports(&mut self, portmap: u16, mount: u16, nfs: u16) {
            let updating = self.updating;
            self.updating = true;
            set_text(self.controls.portmap, &portmap.to_string());
            set_text(self.controls.mount, &mount.to_string());
            set_text(self.controls.nfs, &nfs.to_string());
            self.updating = updating;
        }

        unsafe fn screen_ports(&self) -> Result<(u16, u16, u16), String> {
            let parse = |handle, label| {
                let text = get_text(handle);
                let text = text.trim();
                if text.is_empty() {
                    return Err(format!("{label} 端口不能为空，请填写 1–65535。"));
                }
                if !text.bytes().all(|value| value.is_ascii_digit()) {
                    return Err(format!("{label} 端口只能填写数字，范围为 1–65535。"));
                }
                text.parse::<u16>().ok().filter(|port| *port != 0)
                    .ok_or_else(|| format!("{label} 端口必须在 1–65535 之间。"))
            };
            let ports = [
                ("Portmapper", parse(self.controls.portmap, "Portmapper")?),
                ("MOUNT", parse(self.controls.mount, "MOUNT")?),
                ("NFS", parse(self.controls.nfs, "NFS")?),
            ];
            for (index, (name, port)) in ports.iter().enumerate() {
                for (other_name, other_port) in &ports[index + 1..] {
                    if port == other_port {
                        return Err(format!("{name} 和 {other_name} 端口重复（{port}），请使用不同端口。"));
                    }
                }
            }
            Ok((ports[0].1, ports[1].1, ports[2].1))
        }

        unsafe fn screen_config(&self) -> Result<Config, String> {
            let mut config = self.config.clone();
            let (portmap, mount, nfs) = self.screen_ports()?;
            config.portmap_port = portmap;
            config.mount_port = mount;
            config.nfs_port = nfs;
            config.bind_address = get_text(self.controls.bind).trim().to_owned();
            config.allowed_clients = get_text(self.controls.clients)
                .split([',', '，', ';', '；', '\n'])
                .map(str::trim).filter(|value| !value.is_empty())
                .map(str::to_owned).collect();
            Ok(config)
        }

        unsafe fn persist(&mut self) -> bool {
            let config = match self.screen_config() {
                Ok(config) => config,
                Err(error) => { self.error(&error); return false; }
            };
            if self.config_load_failed && self.config_path.exists() {
                let backup = self.config_path.with_extension(format!("json.invalid-{}",
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos()).unwrap_or_default()));
                if let Err(error) = std::fs::copy(&self.config_path, &backup) {
                    self.error(&format!("原配置无法读取，且备份失败，因此没有覆盖它：{error}"));
                    return false;
                }
                self.log(format!("无法解析的原配置已备份到 {}。", backup.display()));
                self.config_load_failed = false;
            }
            if let Err(error) = config.save(&self.config_path) {
                self.error(&format!("无法保存配置：{error}\n\n请将整个程序文件夹放到有写入权限的位置。"));
                return false;
            }
            self.config = config;
            true
        }

        unsafe fn save_before_close(&mut self) -> bool {
            if self.controls.bind.is_null() || self.config_load_failed { return true; }
            // Closing must remain possible when input is incomplete or the
            // configuration directory is not writable. Use one prompt with
            // the cause; the default choice keeps the window open for editing.
            let result = self.screen_config().and_then(|config| {
                config.save(&self.config_path)
                    .map_err(|error| format!("设置未保存：{error:#}"))?;
                self.config = config;
                Ok(())
            });
            match result {
                Ok(()) => true,
                Err(error) => MessageBoxW(self.hwnd,
                    wide(&format!("{error}\n\n放弃未保存修改并退出？\n选择“否”返回继续编辑。")).as_ptr(),
                    wide("RustNFS").as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) == IDYES,
            }
        }

        unsafe fn save_settings(&mut self) {
            if self.child.is_some() { return; }
            if self.persist() {
                self.update_command();
                self.log(format!("设置已保存。端口：Portmapper {} / MOUNT {} / NFS {}。",
                    self.config.portmap_port, self.config.mount_port, self.config.nfs_port));
            }
        }

        unsafe fn reset_ports(&mut self) {
            if self.child.is_some() { return; }
            let defaults = Config::default();
            self.set_ports(defaults.portmap_port, defaults.mount_port, defaults.nfs_port);
            self.update_command();
            self.log("端口已恢复为 111 / 20048 / 2049；点击“保存设置”或“启动服务”后生效。");
        }

        unsafe fn editor_export(&self) -> Result<ExportConfig, String> {
            let name = get_text(self.controls.name).trim().trim_start_matches('/').to_owned();
            if name.is_empty() || name.len() > 64 ||
                !name.bytes().all(|value| value.is_ascii_alphanumeric() || b"_-".contains(&value)) {
                return Err("共享名称请使用 1–64 个英文字母、数字、下划线或连字符，例如 share。".to_owned());
            }
            let path = get_text(self.controls.path).trim().to_owned();
            if path.is_empty() || !Path::new(&path).is_absolute() || !Path::new(&path).is_dir() {
                return Err("请选择一个实际存在的本地文件夹，并使用完整路径，例如 C:\\nfs。".to_owned());
            }
            let mut share = self.selected.and_then(|index| self.config.exports.get(index))
                .cloned().unwrap_or(ExportConfig {
                    name: String::new(), path: String::new(), read_only: true,
                    uid: 0, gid: 0, file_mode: 0o755, dir_mode: 0o755,
                });
            share.name = name;
            share.path = path;
            share.read_only = SendMessageW(self.controls.readonly, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            Ok(share)
        }

        unsafe fn edit_export(&mut self, replace: bool) {
            if self.child.is_some() { return; }
            let share = match self.editor_export() {
                Ok(share) => share,
                Err(error) => { self.error(&error); return; }
            };
            if replace && self.selected.is_none() { return; }
            if self.config.exports.iter().enumerate().any(|(index, existing)|
                existing.name.eq_ignore_ascii_case(&share.name) && !(replace && self.selected == Some(index))) {
                self.error("这个共享名称已经存在，请换一个名称或点击“更新选中”。");
                return;
            }
            let index = if replace {
                let index = self.selected.unwrap_or(0);
                self.config.exports[index] = share;
                index
            } else {
                if self.config.exports.len() >= 32 {
                    self.error("最多添加 32 个共享目录。"); return;
                }
                self.config.exports.push(share);
                self.config.exports.len() - 1
            };
            self.refresh_list();
            self.select(Some(index));
            if self.persist() { self.log("共享配置已保存。"); }
        }

        unsafe fn remove_export(&mut self) {
            if self.child.is_some() { return; }
            if let Some(index) = self.selected {
                let share = self.config.exports.remove(index);
                self.refresh_list();
                self.select(if self.config.exports.is_empty() { None } else {
                    Some(index.min(self.config.exports.len() - 1))
                });
                self.persist();
                self.log(format!("已移除 /{} 的共享配置，本地文件未删除。", share.name));
            }
        }

        unsafe fn browse(&mut self) {
            let mut info: BROWSEINFOW = std::mem::zeroed();
            let title = wide("选择要共享给开发板的本地文件夹");
            info.hwndOwner = self.hwnd;
            info.lpszTitle = title.as_ptr();
            info.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX;
            let item = SHBrowseForFolderW(&info);
            if !item.is_null() {
                let mut buffer = vec![0u16; 32768];
                if SHGetPathFromIDListEx(item, buffer.as_mut_ptr(), buffer.len() as u32, 0) != 0 {
                    let end = buffer.iter().position(|value| *value == 0).unwrap_or(buffer.len());
                    set_text(self.controls.path, &String::from_utf16_lossy(&buffer[..end]));
                }
                CoTaskMemFree(item.cast());
            }
        }

        unsafe fn mount_command(&self, udp: bool) -> Result<String, String> {
            let index = self.selected.or_else(|| if self.config.exports.is_empty() { None } else { Some(0) })
                .ok_or_else(|| "添加一个共享目录后，这里会显示挂载命令。".to_owned())?;
            let config = self.screen_config()?;
            let share = config.exports.get(index)
                .ok_or_else(|| "请重新选择共享目录。".to_owned())?;
            let bind = get_text(self.controls.bind);
            let entered_host = get_text(self.controls.host);
            let host = if bind != "0.0.0.0" && bind.parse::<Ipv4Addr>().is_ok() {
                bind
            } else { entered_host.trim().to_owned() };
            let host = if host.parse::<Ipv4Addr>().is_ok_and(|ip| !ip.is_unspecified()) {
                host
            } else { "<本机IPv4>".to_owned() };
            Ok(format!("mkdir -p /mnt/nfs\r\n{}", config.mount_command(&host, &share.name, udp)))
        }

        unsafe fn update_command(&self) {
            let command = self.mount_command(false);
            let enabled = command.is_ok() as i32;
            set_text(self.controls.command, &command.unwrap_or_else(|error| error));
            EnableWindow(self.controls.copy_tcp, enabled);
            EnableWindow(self.controls.copy_udp, enabled);
        }

        unsafe fn copy_command(&mut self, udp: bool) {
            let command = match self.mount_command(udp) {
                Ok(command) => command,
                Err(error) => { self.error(&error); return; }
            };
            if command.contains("<本机IPv4>") {
                self.error("请先填写“本机 IPv4”，再复制挂载命令。"); return;
            }
            if OpenClipboard(self.hwnd) == 0 {
                self.error("剪贴板暂时被占用，请稍后再试。"); return;
            }
            let value = wide(&command);
            let memory = GlobalAlloc(GMEM_MOVEABLE, value.len() * 2);
            if memory.is_null() {
                CloseClipboard(); self.error("无法分配剪贴板内存。"); return;
            }
            let destination = GlobalLock(memory).cast::<u16>();
            if destination.is_null() {
                GlobalFree(memory); CloseClipboard(); self.error("无法写入剪贴板。"); return;
            }
            std::ptr::copy_nonoverlapping(value.as_ptr(), destination, value.len());
            GlobalUnlock(memory);
            let empty_ok = EmptyClipboard() != 0;
            let transferred = empty_ok && !SetClipboardData(13, memory).is_null(); // CF_UNICODETEXT
            if !transferred { GlobalFree(memory); }
            CloseClipboard();
            if transferred { self.log(format!("已复制 {} 挂载命令。", if udp { "UDP" } else { "TCP" })); }
            else { self.error("复制命令失败，请重试。"); }
        }

        unsafe fn lock_controls(&self, running: bool) {
            let c = self.controls;
            for control in [c.bind, c.clients, c.portmap, c.mount, c.nfs, c.reset_ports, c.save,
                c.list, c.path, c.browse, c.name, c.readonly, c.add, c.clear] {
                EnableWindow(control, (!running) as i32);
            }
            EnableWindow(c.update, (!running && self.selected.is_some()) as i32);
            EnableWindow(c.remove, (!running && self.selected.is_some()) as i32);
            EnableWindow(c.start, (!running) as i32);
            EnableWindow(c.stop, (running && self.stop_deadline.is_none()) as i32);
        }

        unsafe fn start(&mut self) {
            if self.child.is_some() { return; }
            if let Some(index) = self.selected {
                if let Ok(edited) = self.editor_export() {
                    let saved = &self.config.exports[index];
                    if edited.name != saved.name || edited.path != saved.path || edited.read_only != saved.read_only {
                        self.error("共享目录的修改尚未应用。\n\n请先点击“更新选中”，或重新选中列表中的条目以取消修改。");
                        return;
                    }
                } else if !get_text(self.controls.path).is_empty() {
                    self.error("共享目录编辑内容无效。请修正后点击“更新选中”，或重新选中列表中的条目。");
                    return;
                }
            } else if !get_text(self.controls.path).trim().is_empty() {
                self.error("请先点击“添加共享”，再启动服务。"); return;
            }
            let config = match self.screen_config() {
                Ok(config) => config,
                Err(error) => { self.error(&error); return; }
            };
            if let Err(error) = config.validate() {
                self.error(&format!("配置需要调整：\n\n{error}")); return;
            }
            self.config = config;
            if !self.persist() { return; }
            let binary = self.directory.join("rustnfsd.exe");
            if !binary.is_file() {
                self.error("没有找到 rustnfsd.exe。\n\n请完整解压程序包，将它和图形界面放在同一文件夹。"); return;
            }
            while self.events.try_recv().is_ok() {}
            let spawned = Command::new(binary)
                .arg("--config").arg(&self.config_path)
                .current_dir(&self.directory)
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .spawn();
            match spawned {
                Ok(mut child) => {
                    if let Some(stdout) = child.stdout.take() { read_output(stdout, self.sender.clone()); }
                    if let Some(stderr) = child.stderr.take() { read_output(stderr, self.sender.clone()); }
                    self.child = Some(child);
                    self.ready = false;
                    self.stop_deadline = None;
                    set_text(self.controls.status, "● 正在启动…");
                    self.lock_controls(true);
                    self.log("正在启动服务，等待全部端口监听成功…");
                }
                Err(error) => self.error(&format!("无法启动服务：{error}")),
            }
        }

        unsafe fn stop(&mut self) {
            if self.stop_deadline.is_some() { return; }
            if let Some(child) = self.child.as_mut() {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(b"stop\n");
                    let _ = stdin.flush();
                }
                self.stop_deadline = Some(Instant::now() + Duration::from_secs(3));
                set_text(self.controls.status, "● 正在停止…");
                self.lock_controls(true);
                self.log("正在关闭共享服务…");
            }
        }

        unsafe fn tick(&mut self) -> bool {
            let mut log_changed = false;
            for _ in 0..128 {
                match self.events.try_recv() {
                    Ok(Event::Ready) => {
                        if self.child.is_some() && self.stop_deadline.is_none() {
                            self.ready = true;
                            set_text(self.controls.status, "● 正在运行");
                            self.log("服务已就绪。可在开发板执行上面的挂载命令。");
                        }
                    }
                    Ok(Event::Line(line)) => { self.push_log(line); log_changed = true; }
                    Err(_) => break,
                }
            }
            if log_changed { self.render_log(); }
            let mut exit = None;
            let mut killed = false;
            if let Some(child) = self.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => exit = Some(status.to_string()),
                    Ok(None) => {
                        if self.stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            if child.kill().is_ok() { killed = true; }
                            self.stop_deadline = Some(Instant::now() + Duration::from_secs(1));
                        }
                    }
                    Err(error) => {
                        // A failed status check must not make us forget a live child.
                        self.log(format!("读取服务进程状态失败：{error}"));
                    }
                }
            }
            if killed { self.log("服务未及时停止，已终止本界面创建的进程。"); }
            if let Some(status) = exit {
                let expected = self.stop_deadline.is_some();
                self.child = None;
                self.ready = false;
                self.stop_deadline = None;
                set_text(self.controls.status, if expected { "● 已停止" } else { "● 服务已退出" });
                self.lock_controls(false);
                self.log(format!("服务进程已退出（{status}）。"));
            }
            self.closing && self.child.is_none()
        }

        unsafe fn help(&self) {
            for filename in ["README.txt", "使用说明.txt", "README.html", "README.md"] {
                let path = self.directory.join(filename);
                if path.is_file() {
                    let result = ShellExecuteW(self.hwnd, wide("open").as_ptr(),
                        wide(&path.to_string_lossy()).as_ptr(), null(), null(), SW_SHOWNORMAL);
                    if result as isize > 32 { return; }
                }
            }
            MessageBoxW(self.hwnd, wide(
                "1. 浏览本地文件夹，填写共享名称，点击“添加共享”。\n\n\
                 2. 填写开发板 IP，例如 192.168.1.50/32。确认本机 IPv4 是开发板能够访问的网卡地址。\n\n\
                 3. 点击“启动服务”，等待右上角显示“正在运行”。如有防火墙提示，按说明允许所需端口。\n\n\
                 4. 复制 TCP 挂载命令，在开发板执行。旧设备需要时再尝试 UDP。\n\n\
                 本版使用 NFSv3；不提供 NFSv2、NLM 文件锁或完整 Linux 根文件系统语义。\n\n\
                 关闭窗口会停止共享；服务重启后客户端需要重新挂载。"
            ).as_ptr(), wide("RustNFS 使用说明").as_ptr(), MB_OK | MB_ICONINFORMATION);
        }

        unsafe fn command(&mut self, id: u16, notification: u16) {
            if self.updating { return; }
            if id == ID_LIST && notification == LBN_SELCHANGE as u16 {
                let selected = SendMessageW(self.controls.list, LB_GETCURSEL, 0, 0);
                self.select(if selected >= 0 { Some(selected as usize) } else { None });
                return;
            }
            if [ID_HOST, ID_BIND, ID_PORTMAP, ID_MOUNT, ID_NFS].contains(&id) && notification == EN_CHANGE as u16 {
                self.update_command(); return;
            }
            if notification != BN_CLICKED as u16 { return; }
            match id {
                ID_BROWSE if self.child.is_none() => self.browse(),
                ID_ADD => self.edit_export(false),
                ID_UPDATE => self.edit_export(true),
                ID_REMOVE => self.remove_export(),
                ID_CLEAR if self.child.is_none() => self.select(None),
                ID_RESET_PORTS => self.reset_ports(),
                ID_SAVE => self.save_settings(),
                ID_START => self.start(),
                ID_STOP => self.stop(),
                ID_HELP => self.help(),
                ID_TCP => self.copy_command(false),
                ID_UDP => self.copy_command(true),
                _ => {},
            }
        }
    }

    impl Drop for App {
        fn drop(&mut self) {
            // Normal shutdown already waits up to three seconds through WM_TIMER.
            // This is a final guard for unexpected window destruction.
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            unsafe {
                for font in [self.font, self.title_font, self.mono_font] {
                    if !font.is_null() { DeleteObject(font); }
                }
            }
        }
    }

    fn read_output(reader: impl std::io::Read + Send + 'static, sender: SyncSender<Event>) {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                let mut line = Vec::with_capacity(256);
                // Process trusted backend output in bounded chunks even if a line is missing a newline.
                let read = reader.by_ref().take(8192).read_until(b'\n', &mut line);
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line).trim_end().to_owned();
                        if text.contains("RUSTNFS_READY") && sender.send(Event::Ready).is_err() { break; }
                        match sender.try_send(Event::Line(text)) {
                            Ok(()) | Err(mpsc::TrySendError::Full(_)) => {},
                            Err(mpsc::TrySendError::Disconnected(_)) => break,
                        }
                    }
                }
            }
        });
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if message == WM_NCDESTROY {
            let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !pointer.is_null() { drop(Box::from_raw(pointer)); }
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if pointer.is_null() { return DefWindowProcW(hwnd, message, wparam, lparam); }
        match message {
            WM_SIZE => { (*pointer).layout(); 0 }
            WM_COMMAND => {
                (*pointer).command((wparam & 0xffff) as u16, ((wparam >> 16) & 0xffff) as u16); 0
            }
            WM_TIMER if wparam == TIMER => {
                let close = (*pointer).tick();
                if close { DestroyWindow(hwnd); }
                0
            }
            WM_CLOSE => {
                if (*pointer).child.is_some() {
                    (*pointer).closing = true;
                    (*pointer).stop();
                } else {
                    // Persist connection edits without requiring a complete configuration.
                    if (*pointer).save_before_close() { DestroyWindow(hwnd); }
                }
                0
            }
            WM_DESTROY => { KillTimer(hwnd, TIMER); PostQuitMessage(0); 0 }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam as *mut MINMAXINFO);
                info.ptMinTrackSize.x = (*pointer).px(840);
                info.ptMinTrackSize.y = (*pointer).px(780);
                0
            }
            WM_DPICHANGED => {
                (*pointer).dpi = (wparam & 0xffff) as u32;
                (*pointer).fonts();
                let mut child = GetWindow(hwnd, GW_CHILD);
                while !child.is_null() {
                    SendMessageW(child, WM_SETFONT, (*pointer).font as usize, 1);
                    child = GetWindow(child, GW_HWNDNEXT);
                }
                SendMessageW((*pointer).controls.title, WM_SETFONT, (*pointer).title_font as usize, 1);
                SendMessageW((*pointer).controls.command, WM_SETFONT, (*pointer).mono_font as usize, 1);
                SendMessageW((*pointer).controls.log, WM_SETFONT, (*pointer).mono_font as usize, 1);
                let rect = &*(lparam as *const RECT);
                SetWindowPos(hwnd, null_mut(), rect.left, rect.top,
                    rect.right - rect.left, rect.bottom - rect.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let dc = wparam as HDC;
                let control = lparam as HWND;
                let muted = [(*pointer).controls.subtitle, (*pointer).controls.clients_hint,
                    (*pointer).controls.shares_hint, (*pointer).controls.service_hint].contains(&control);
                let color = if control == (*pointer).controls.status && (*pointer).ready {
                    0x00546027
                } else if muted { MUTED } else { TEXT };
                SetTextColor(dc, color);
                SetBkMode(dc, TRANSPARENT as i32);
                (*pointer).background as isize
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                let dc = wparam as HDC;
                SetTextColor(dc, TEXT); SetBkColor(dc, 0x00ffffff);
                GetStockObject(WHITE_BRUSH) as isize
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    pub fn run() -> Result<(), String> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let _ = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
            let common = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_STANDARD_CLASSES,
            };
            InitCommonControlsEx(&common);
            let instance = GetModuleHandleW(null());
            let class_name = wide("RustNFS.Native.Manager.0.1");
            let background = CreateSolidBrush(BG);
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
                hbrBackground: background,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            if RegisterClassW(&class) == 0 {
                DeleteObject(background); CoUninitialize();
                return Err("注册 Windows 窗口失败。".to_owned());
            }
            let directory = std::env::current_exe().ok().and_then(|path| path.parent().map(Path::to_owned))
                .ok_or_else(|| "无法确定程序所在文件夹。".to_owned())?;
            let mut app = Box::new(App::new(instance, directory, background));
            let dpi = GetDpiForSystem().max(96);
            let mut rect = RECT { left: 0, top: 0, right: 860 * dpi as i32 / 96, bottom: 790 * dpi as i32 / 96 };
            AdjustWindowRectExForDpi(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0, dpi);
            let hwnd = CreateWindowExW(0, class_name.as_ptr(), wide(concat!("RustNFS ", env!("CARGO_PKG_VERSION"))).as_ptr(),
                WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT,
                rect.right - rect.left, rect.bottom - rect.top,
                null_mut(), null_mut(), instance, null());
            if hwnd.is_null() {
                UnregisterClassW(class_name.as_ptr(), instance);
                DeleteObject(background); CoUninitialize();
                return Err("创建 RustNFS 窗口失败。".to_owned());
            }
            app.hwnd = hwnd;
            let pointer = Box::into_raw(app);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
            if let Err(error) = (*pointer).init_controls() {
                DestroyWindow(hwnd); UnregisterClassW(class_name.as_ptr(), instance);
                DeleteObject(background); CoUninitialize();
                return Err(error);
            }
            ShowWindow(hwnd, SW_SHOWNORMAL);
            UpdateWindow(hwnd);
            let mut message: MSG = std::mem::zeroed();
            loop {
                let result = GetMessageW(&mut message, null_mut(), 0, 0);
                if result <= 0 { break; }
                if IsDialogMessageW(hwnd, &message) == 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            UnregisterClassW(class_name.as_ptr(), instance);
            DeleteObject(background);
            CoUninitialize();
            Ok(())
        }
    }
}
