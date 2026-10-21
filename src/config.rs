use anyhow::{Context, Result, bail};
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, io::Write, net::{IpAddr, Ipv4Addr}, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bind_address: String,
    pub portmap_port: u16,
    pub mount_port: u16,
    pub nfs_port: u16,
    pub allowed_clients: Vec<String>,
    pub exports: Vec<ExportConfig>,
    pub max_connections: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExportConfig {
    pub name: String,
    pub path: String,
    pub read_only: bool,
    pub uid: u32,
    pub gid: u32,
    pub file_mode: u32,
    pub dir_mode: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".into(), portmap_port: 111,
            mount_port: 20048, nfs_port: 2049,
            allowed_clients: vec!["127.0.0.1/32".into()],
            exports: Vec::new(), max_connections: 32,
        }
    }
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self { name: "share".into(), path: String::new(), read_only: true,
            uid: 0, gid: 0, file_mode: 0o755, dir_mode: 0o755 }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("无法读取配置：{}", path.display()))?;
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        serde_json::from_slice(bytes).context("配置格式错误，请检查 JSON 字段与逗号")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        let temp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&temp).context("无法写入配置，请将程序解压到可写目录")?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path).context("无法保存配置")?;
        Ok(())
    }

    pub fn networks(&self) -> Result<Vec<Ipv4Net>> {
        self.allowed_clients.iter().map(|s| {
            let s = s.trim();
            if s.contains('/') {
                s.parse::<Ipv4Net>().with_context(|| format!("无效的 IPv4 网段：{s}"))
            } else {
                let ip = s.parse::<Ipv4Addr>().with_context(|| format!("无效的客户端 IPv4：{s}"))?;
                Ok(Ipv4Net::new(ip, 32)?)
            }
        }).collect()
    }

    pub fn validate(&self) -> Result<()> {
        self.bind_address.parse::<Ipv4Addr>().context("监听地址必须是 IPv4，例如 0.0.0.0")?;
        if [self.portmap_port, self.mount_port, self.nfs_port].contains(&0) {
            bail!("端口不能为 0");
        }
        if HashSet::from([self.portmap_port, self.mount_port, self.nfs_port]).len() != 3 {
            bail!("Portmapper、MOUNT、NFS 端口必须各不相同");
        }
        if self.allowed_clients.is_empty() || self.allowed_clients.len() > 64 {
            bail!("请填写至少一个允许访问的设备 IPv4 或网段，最多 64 条");
        }
        let _ = self.networks()?;
        if !(1..=128).contains(&self.max_connections) {
            bail!("max_connections 必须在 1 到 128 之间");
        }
        if self.exports.is_empty() || self.exports.len() > 32 {
            bail!("请添加 1 到 32 个共享目录");
        }
        let mut names = HashSet::new();
        for e in &self.exports {
            if e.name.is_empty() || e.name.len() > 64 ||
                !e.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                bail!("共享名称只允许英文字母、数字、下划线和连字符，长度 1–64：{}", e.name);
            }
            if !names.insert(e.name.to_ascii_lowercase()) {
                bail!("共享名称重复：{}", e.name);
            }
            if e.file_mode > 0o777 || e.dir_mode > 0o777 {
                bail!("权限值只支持 000–777（JSON 中使用十进制数值）");
            }
            let path = Path::new(&e.path);
            if !path.is_absolute() || !path.is_dir() {
                bail!("共享目录必须是已存在的绝对路径：{}", e.path);
            }
        }
        Ok(())
    }

    pub fn allows(&self, addr: IpAddr) -> bool {
        let IpAddr::V4(ip) = addr else { return false; };
        self.networks().is_ok_and(|nets| nets.iter().any(|net| net.contains(&ip)))
    }

    pub fn mount_command(&self, host: &str, export: &str, udp: bool) -> String {
        let protocol = if udp { "udp" } else { "tcp" };
        format!("mount -t nfs -o vers=3,{protocol},mountproto={protocol},nolock,port={},mountport={},rsize=8192,wsize=8192 {host}:/{export} /mnt/nfs", self.nfs_port, self.mount_port)
    }
}
