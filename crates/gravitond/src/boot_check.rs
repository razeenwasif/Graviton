//! `gravitond boot-check`: milestone 1's end-to-end test.
//!
//! Boots a guest VM, waits for the agent's `ready` over vsock, runs a command
//! in the guest, streams its output, then asks the agent to power off.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use graviton_core::paths;
use graviton_proto::{AGENT_VSOCK_PORT, AgentEvent, AgentRequest, decode_line, encode_line};
use graviton_vmm::{VmConfig, VmHandle};

#[derive(Args, Debug)]
pub struct BootCheckArgs {
    /// Kernel image (default: <guest dir>/vmlinux)
    #[arg(long)]
    kernel: Option<PathBuf>,
    /// Initramfs (default: <guest dir>/initrd.img)
    #[arg(long)]
    initrd: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u32,
    /// Memory in MiB.
    #[arg(long, default_value_t = 512)]
    memory: u64,
    /// Command to run in the guest.
    #[arg(last = true, default_values_t = ["/bin/busybox".to_string(), "uname".to_string(), "-a".to_string()])]
    command: Vec<String>,
}

pub fn run(args: BootCheckArgs) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let guest = paths::guest_dir();
        let mut config = VmConfig::new(
            args.kernel.unwrap_or_else(|| guest.join("vmlinux")),
            args.initrd.unwrap_or_else(|| guest.join("initrd.img")),
            "",
        );
        config.cpus = args.cpus;
        config.memory_mib = args.memory;
        let vm_dir = paths::vms_dir().join("boot-check");
        std::fs::create_dir_all(&vm_dir).with_context(|| format!("creating {}", vm_dir.display()))?;
        config.console_log = Some(vm_dir.join("console.log"));

        let t0 = Instant::now();
        let vm = graviton_vmm::apple::AppleHypervisor::new().boot_vm(&config)?;
        println!("VM started            {:>6} ms", t0.elapsed().as_millis());

        let result = talk_to_agent(&vm, args.command, t0);
        if result.is_err() {
            let _ = vm.kill();
            if let Ok(log) = std::fs::read_to_string(vm.console_log()) {
                eprintln!("--- guest console ({}) ---", vm.console_log().display());
                for line in log.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev() {
                    eprintln!("{line}");
                }
            }
            return result;
        }
        match vm.wait_stopped(Duration::from_secs(10)) {
            Ok(()) => println!("Guest powered off      {:>6} ms", t0.elapsed().as_millis()),
            Err(_) => {
                vm.kill()?;
                println!("Guest didn't power off; stopped it");
            }
        }
        println!("Console log: {}", vm.console_log().display());
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        bail!("boot-check needs macOS (Virtualization.framework)")
    }
}

fn talk_to_agent(vm: &dyn VmHandle, command: Vec<String>, t0: Instant) -> Result<()> {
    let fd = vm.connect(AGENT_VSOCK_PORT)?;
    let mut writer = File::from(fd);
    let mut reader = BufReader::new(writer.try_clone()?);
    let mut line = String::new();

    let mut next_event = |reader: &mut BufReader<File>| -> Result<AgentEvent> {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            bail!("agent closed the connection");
        }
        Ok(decode_line(line.as_bytes())?)
    };

    match next_event(&mut reader)? {
        AgentEvent::Ready { kernel } => println!("Agent ready            {:>6} ms   (Linux {kernel})", t0.elapsed().as_millis()),
        other => bail!("expected ready from the agent, got {other:?}"),
    }

    println!("$ {}", command.join(" "));
    writer.write_all(&encode_line(&AgentRequest::Exec {
        argv: command,
        env: vec![],
        cwd: "/".into(),
        tty: false,
        container: None,
        user: None,
        hostname: None,
        rows: 0,
        cols: 0,
        stdin: None,
        mounts: vec![],
    }))?;
    let code = loop {
        match next_event(&mut reader)? {
            AgentEvent::Started { .. } => {}
            AgentEvent::Stdout { data, .. } | AgentEvent::Stderr { data, .. } => {
                std::io::stdout().write_all(&data)?;
            }
            AgentEvent::Exited { code, .. } => break code,
            AgentEvent::Error { message } => bail!("agent: {message}"),
            _ => {}
        }
    };
    println!("Command exited {code}       {:>6} ms", t0.elapsed().as_millis());

    writer.write_all(&encode_line(&AgentRequest::Shutdown))?;
    Ok(())
}
