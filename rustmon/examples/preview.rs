//! Manual preview of chunks 1-4: not part of the chunked build plan, and not
//! a substitute for chunk 7's real `--once` binary. Takes two snapshots a
//! second apart with the CPU and memory collectors and prints what's
//! actually implemented so far.
//!
//! Run with `cargo run -p rustmon --example preview`.

use std::thread::sleep;
use std::time::Duration;

use rustmon::collector::Registry;
use rustmon::collectors::cpu::CpuCollector;
use rustmon::collectors::disk::DiskCollector;
use rustmon::collectors::memory::MemoryCollector;
use rustmon::collectors::net::NetCollector;
use rustmon::collectors::thermal::ThermalCollector;
use rustmon::delta::RateTracker;
use rustmon::sysfs::SysfsReader;

fn main() {
    let mut registry = Registry::from_collectors(
        vec![
            Box::new(CpuCollector::new()),
            Box::new(MemoryCollector::new()),
            Box::new(ThermalCollector::new()),
            Box::new(DiskCollector::new()),
            Box::new(NetCollector::new()),
        ],
        SysfsReader::new(),
    );
    let mut tracker = RateTracker::new();

    let first = registry.collect_all().expect("first snapshot");
    tracker.update(&first);

    println!("took a first snapshot, waiting 1s for a rate...\n");
    sleep(Duration::from_secs(1));

    let second = registry.collect_all().expect("second snapshot");
    let rates = tracker.update(&second).expect("valid interval");

    if !second.errors.is_empty() {
        println!("collector errors: {:?}\n", second.errors);
    }

    if let Some(cpu) = &second.cpu {
        println!("CPU");
        if let Some(model) = &cpu.model {
            println!("  model      : {model}");
        }
        println!("  cores      : {}", cpu.per_core.len());
        if let Some(load) = cpu.load_avg {
            println!("  load avg   : {:.2} {:.2} {:.2}", load[0], load[1], load[2]);
        }
        if let Some(busy) = rates.cpu_total {
            println!("  busy       : {:.1}%", busy.as_f64());
        }
        for (i, freq) in cpu.freq_khz.iter().enumerate().take(4) {
            match freq {
                Some(f) => println!("  core{i} freq : {:.2} GHz", f.as_ghz()),
                None => println!("  core{i} freq : n/a"),
            }
        }
        if cpu.freq_khz.len() > 4 {
            println!("  ... ({} more cores)", cpu.freq_khz.len() - 4);
        }
    }

    if let Some(mem) = &second.memory {
        println!("\nMemory");
        println!("  total      : {}", mem.total);
        println!(
            "  used       : {} ({:.1}%)",
            mem.used(),
            mem.used_percent().map(|p| p.as_f64()).unwrap_or(0.0)
        );
        println!("  available  : {}", mem.available);
        println!("  swap used  : {} / {}", mem.swap_used(), mem.swap_total);
    }

    if let Some(thermal) = &second.thermal {
        println!("\nThermal ({} chips)", thermal.chips.len());
        for chip in &thermal.chips {
            println!("  {}", chip.name);
            for t in &chip.temps {
                let trip = match (t.max, t.crit) {
                    (Some(max), Some(crit)) => format!(" (max {:.1}, crit {:.1})", max.as_celsius(), crit.as_celsius()),
                    (Some(max), None) => format!(" (max {:.1})", max.as_celsius()),
                    (None, Some(crit)) => format!(" (crit {:.1})", crit.as_celsius()),
                    (None, None) => String::new(),
                };
                println!("    {:24} {:6.1} C  [{:?}]{trip}", t.label, t.value.as_celsius(), t.severity());
            }
            for f in &chip.fans {
                println!("    {:24} {} rpm", f.label, f.rpm.as_u64());
            }
        }
    }

    if let Some(disk) = &second.disks {
        println!("\nDisk ({} devices, {} mounts)", disk.devices.len(), disk.mounts.len());
        for d in &disk.devices {
            let r = rates.disk.get(&d.name);
            match r {
                Some(r) => println!(
                    "  {:10} read {:>10} write {:>10} util {:>5}",
                    d.name,
                    r.read.human(),
                    r.write.human(),
                    r.utilisation.map(|p| format!("{:.1}%", p.as_f64())).unwrap_or_else(|| "n/a".into())
                ),
                None => println!("  {:10} (no rate yet)", d.name),
            }
        }
        for m in &disk.mounts {
            println!("  mount {} -> {} ({})", m.source, m.mount_point, m.fs_type);
        }
    }

    if let Some(net) = &second.net {
        println!("\nNet ({} interfaces)", net.interfaces.len());
        for i in &net.interfaces {
            let r = rates.net.get(&i.name);
            let (rx, tx) = r.map(|r| (r.rx.human(), r.tx.human())).unwrap_or_else(|| ("n/a".into(), "n/a".into()));
            println!(
                "  {:12} rx {:>10} tx {:>10} state={:?} mtu={:?}",
                i.name, rx, tx, i.operstate, i.mtu
            );
        }
    }

    println!("\n(interval: {:.3}s)", rates.interval_secs);
}
