use anyhow::{Context, Result, bail};
use rustnfs::{config::Config, filesystem::{LocalFs, MAX_IO}};
use nfs3_server::transport::{self, ServerConfig};
use std::{io::{self, BufRead, Write}, path::PathBuf, sync::Arc, time::Duration};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,rustnfs=info,nfs3_server::transport=info".into()))
        .with_ansi(false).with_target(false).with_writer(io::stderr).init();
    if let Err(error) = run().await {
        eprintln!("RUSTNFS_ERROR: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut path = std::env::current_exe()?.parent().context("找不到程序目录")?.join("rustnfs.json");
    let mut check = false;
    let mut init = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--config" => path = PathBuf::from(args.next().context("--config 后需要配置文件路径")?),
            "--check" => check = true,
            "--init" => init = true,
            "--version" | "-V" => { println!("RustNFS {} | NFSv3 TCP/UDP", env!("CARGO_PKG_VERSION")); return Ok(()); }
            "--help" | "-h" => {
                println!("RustNFS {}\n\n用法：rustnfsd --config rustnfs.json [--check]\n      rustnfsd --config rustnfs.json --init\n\n--check   检查配置和目录，不启动网络服务\n--init    创建初始配置；已有文件不会覆盖\n\n停止：Ctrl+C，或通过标准输入发送 stop。\n协议：NFSv3 TCP/UDP；客户端使用 vers=3,nolock。", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => bail!("未知参数：{argument}（使用 --help 查看用法）"),
        }
    }
    if init {
        if path.exists() { bail!("配置已存在，未覆盖：{}", path.display()); }
        Config::default().save(&path)?;
        println!("已创建 {}；请添加共享目录与客户端地址。", path.display());
        return Ok(());
    }
    let config = Config::load(&path)?;
    config.validate()?;
    let fs = Arc::new(LocalFs::new(&config)?);
    if check { println!("配置有效：{} 个共享目录", config.exports.len()); return Ok(()); }
    let networks = config.networks()?;
    let server_config = ServerConfig {
        bind_ip: config.bind_address.parse()?,
        nfs_port: config.nfs_port, mount_port: config.mount_port, portmap_port: config.portmap_port,
        exports: config.exports.iter().map(|e| format!("/{}", e.name)).collect(),
        allow_client: Arc::new(move |address| match address {
            std::net::IpAddr::V4(ip) => networks.iter().any(|network| network.contains(&ip)),
            _ => false,
        }),
        max_connections: config.max_connections,
        max_concurrent_requests: 16,
        idle_timeout: Duration::from_secs(30),
        max_io_bytes: MAX_IO,
        ..ServerConfig::default()
    };
    let server = transport::start(fs, server_config).await
        .context("服务启动失败；端口被占用时，请停止占用程序，或在“连接设置”中修改对应端口")?;
    println!("RUSTNFS_READY");
    println!("RustNFS {} | NFSv3 TCP/UDP | {}", env!("CARGO_PKG_VERSION"), config.bind_address);
    let ports = server.ports();
    println!("监听端口（TCP/UDP）：Portmapper {} | MOUNT {} | NFS {}",
        ports.portmap_port, ports.mount_port, ports.nfs_port);
    for export in &config.exports {
        println!("/{} -> {} [{}]", export.name, export.path, if export.read_only { "只读" } else { "读写" });
    }
    io::stdout().flush()?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) if line.trim().eq_ignore_ascii_case("stop") => { let _ = sender.blocking_send(()); break; }
                Ok(_) => (), Err(_) => break,
            }
        }
        // An EOF on a detached console should not stop a headless daemon.
    });
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        value = receiver.recv() => {
            if value.is_none() { tokio::signal::ctrl_c().await?; }
        }
    }
    server.shutdown().await.context("服务停止失败")?;
    println!("RUSTNFS_STOPPED");
    Ok(())
}
