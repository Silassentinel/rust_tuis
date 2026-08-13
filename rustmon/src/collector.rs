//! The `Collector` trait and the registry that drives them.
//!
//! Rust Book Ch. 10 (traits) and Ch. 17 (trait objects): which collectors run
//! is a runtime decision — it depends on config *and* on what the machine
//! actually has — so `Box<dyn Collector>` is the right call over static
//! dispatch.
//!
//! The contract that matters: **one collector failing must never stop the
//! others.** A missing GPU or an unreadable hwmon file is the normal case on
//! most machines, not an error condition for the program as a whole.

use crate::collectors::connections::ConnectionsCollector;
use crate::collectors::cpu::CpuCollector;
use crate::collectors::disk::DiskCollector;
use crate::collectors::gpu::GpuCollector;
use crate::collectors::memory::MemoryCollector;
use crate::collectors::net::NetCollector;
use crate::collectors::thermal::ThermalCollector;
use crate::config::Config;
use crate::error::Result;
use crate::sample::{CollectorError, Snapshot};
use crate::sysfs::SysfsReader;

/// Something that reads one area of the system into a [`Snapshot`].
pub trait Collector {
    /// Stable identifier, used in config, in `--only`/`--skip`, and in
    /// [`crate::sample::CollectorError::collector`]. Must match the module name.
    fn name(&self) -> &'static str;

    /// Cheap probe: does this machine expose what this collector reads?
    ///
    /// Called once at registry build time so an absent subsystem costs one
    /// `stat` rather than a failed read on every refresh.
    fn probe(&self, reader: &SysfsReader) -> bool;

    /// Read, parse, and write into `snapshot`.
    ///
    /// Implementations must not panic and must not write to the filesystem.
    /// Expected absences (`ENOENT`, `EACCES`) should leave the field `None`
    /// and return `Ok(())`, not an error.
    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()>;
}

/// The set of collectors to run, in order.
pub struct Registry {
    collectors: Vec<Box<dyn Collector>>,
    reader: SysfsReader,
}

impl Registry {
    /// Build the default set, honouring `config`'s enable/disable lists and
    /// dropping anything whose [`Collector::probe`] returns false.
    ///
    /// Construction happens before the `probe` filter, not instead of it:
    /// `config.wants` is a cheap name check the user controls (`--only`,
    /// `--skip`), while `probe` is what actually asks the machine whether
    /// this collector has anything to read. A `--only gpu` on a machine with
    /// no GPU should yield an empty, still-valid registry, not skip the
    /// question of what's real.
    pub fn from_config(config: &Config, reader: SysfsReader) -> Result<Self> {
        let mut collectors: Vec<Box<dyn Collector>> = Vec::new();

        if config.wants(crate::collectors::cpu::NAME) {
            collectors.push(Box::new(CpuCollector::new()));
        }
        if config.wants(crate::collectors::memory::NAME) {
            collectors.push(Box::new(MemoryCollector::new()));
        }
        if config.wants(crate::collectors::thermal::NAME) {
            collectors.push(Box::new(ThermalCollector::new()));
        }
        if config.wants(crate::collectors::disk::NAME) {
            collectors.push(Box::new(DiskCollector::new()));
        }
        if config.wants(crate::collectors::net::NAME) {
            collectors.push(Box::new(NetCollector::new()));
        }
        if config.wants(crate::collectors::gpu::NAME) {
            collectors.push(Box::new(GpuCollector::new()));
        }
        if config.wants(crate::collectors::connections::NAME) {
            collectors.push(Box::new(ConnectionsCollector::new()));
        }

        collectors.retain(|c| c.probe(&reader));

        Ok(Registry { collectors, reader })
    }

    /// Build from an explicit list — the test entry point.
    pub fn from_collectors(collectors: Vec<Box<dyn Collector>>, reader: SysfsReader) -> Self {
        Registry { collectors, reader }
    }

    /// Names of the collectors that will actually run.
    pub fn active(&self) -> Vec<&'static str> {
        self.collectors.iter().map(|c| c.name()).collect()
    }

    /// Run every collector once.
    ///
    /// Never returns `Err` for a single collector's failure — that goes into
    /// [`Snapshot::errors`] and the rest continue. The `Result` is reserved for
    /// a failure to produce a snapshot at all.
    pub fn collect_all(&mut self) -> Result<Snapshot> {
        let mut snapshot = Snapshot::now();

        // Destructured so the loop can hold `&mut collectors` and `&reader` at
        // once — they're disjoint fields, but spelling that out reads better
        // than relying on the borrow checker's field splitting.
        let Registry { collectors, reader } = self;

        for collector in collectors.iter_mut() {
            if let Err(e) = collector.collect(reader, &mut snapshot) {
                // The whole point of this loop: record and carry on. A machine
                // with no hwmon chips must still report its CPU.
                snapshot.errors.push(CollectorError {
                    collector: collector.name(),
                    message: e.to_string(),
                });
            }
        }

        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::sample::{CpuSample, CpuTimes, MemorySample};

    enum Behaviour {
        SetCpu,
        SetMemory,
        Fail,
    }

    struct FakeCollector {
        name: &'static str,
        behaviour: Behaviour,
    }

    impl Collector for FakeCollector {
        fn name(&self) -> &'static str {
            self.name
        }

        fn probe(&self, _reader: &SysfsReader) -> bool {
            true
        }

        fn collect(&mut self, _reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
            match self.behaviour {
                Behaviour::SetCpu => {
                    snapshot.cpu = Some(CpuSample {
                        total: CpuTimes::default(),
                        per_core: Vec::new(),
                        freq_khz: Vec::new(),
                        load_avg: None,
                        model: None,
                        ctxt: None,
                        btime: None,
                    });
                    Ok(())
                }
                Behaviour::SetMemory => {
                    snapshot.memory = Some(MemorySample::default());
                    Ok(())
                }
                Behaviour::Fail => Err(Error::Unsupported {
                    what: "deliberately failing test collector",
                }),
            }
        }
    }

    fn fake(name: &'static str, behaviour: Behaviour) -> Box<dyn Collector> {
        Box::new(FakeCollector { name, behaviour })
    }

    /// The contract this module exists to hold: a machine with no hwmon chips
    /// must still report its CPU. A failing collector is recorded, not fatal.
    #[test]
    fn one_collector_failing_does_not_stop_the_others() {
        let mut registry = Registry::from_collectors(
            vec![
                fake("cpu", Behaviour::SetCpu),
                fake("thermal", Behaviour::Fail),
                fake("memory", Behaviour::SetMemory),
            ],
            SysfsReader::new(),
        );

        let snapshot = registry
            .collect_all()
            .expect("a collector error must not fail collect_all");

        assert!(snapshot.cpu.is_some(), "collector before the failure ran");
        assert!(snapshot.memory.is_some(), "collector after the failure ran");

        assert_eq!(snapshot.errors.len(), 1);
        assert_eq!(snapshot.errors[0].collector, "thermal");
        assert!(!snapshot.errors[0].message.is_empty());
    }

    #[test]
    fn every_failure_is_recorded_separately() {
        let mut registry = Registry::from_collectors(
            vec![
                fake("disk", Behaviour::Fail),
                fake("net", Behaviour::Fail),
                fake("gpu", Behaviour::Fail),
            ],
            SysfsReader::new(),
        );

        let snapshot = registry.collect_all().expect("still produces a snapshot");

        // A snapshot where everything failed is still a snapshot — the
        // `errors` list is the diagnostic channel, not a failure signal.
        assert_eq!(snapshot.errors.len(), 3);
        assert_eq!(
            snapshot
                .errors
                .iter()
                .map(|e| e.collector)
                .collect::<Vec<_>>(),
            vec!["disk", "net", "gpu"]
        );
        assert!(snapshot.cpu.is_none() && snapshot.memory.is_none());
    }

    #[test]
    fn active_reports_the_registered_collectors_in_order() {
        let registry = Registry::from_collectors(
            vec![
                fake("cpu", Behaviour::SetCpu),
                fake("memory", Behaviour::SetMemory),
            ],
            SysfsReader::new(),
        );

        assert_eq!(registry.active(), vec!["cpu", "memory"]);
    }

    #[test]
    fn an_empty_registry_still_produces_a_snapshot() {
        let mut registry = Registry::from_collectors(Vec::new(), SysfsReader::new());
        let snapshot = registry.collect_all().expect("empty registry is not an error");

        assert!(snapshot.errors.is_empty());
        assert!(snapshot.cpu.is_none());
    }

    // ---- from_config --------------------------------------------------------

    /// `probe()` is what actually decides what runs — a fixture tree with
    /// nothing readable should register zero collectors even though
    /// `Config` asked for everything.
    #[test]
    fn from_config_drops_everything_that_fails_probe() {
        let tree = crate::sysfs::tests::TempTree::new("registry-from-config-empty");
        tree.dir("nothing-here");
        let reader = tree.reader();

        let config = Config::new();
        let registry = Registry::from_config(&config, reader).expect("builds fine");
        assert!(registry.active().is_empty());
    }

    /// `config.wants` (an `--only`/`--skip` name check) is applied before
    /// `probe`, so `--only cpu` on a machine with a readable `/proc/stat`
    /// registers exactly `cpu`, nothing else — even though every other
    /// collector's source file is also present in the fixture.
    #[test]
    fn from_config_honours_only() {
        let tree = crate::sysfs::tests::TempTree::new("registry-from-config-only");
        tree.file("proc/stat", "cpu  0 0 0 0 0 0 0 0 0 0\n");
        tree.file("proc/meminfo", "MemTotal: 1000 kB\n");
        let reader = tree.reader();

        let mut config = Config::new();
        config.only = vec!["cpu".to_string()];

        let registry = Registry::from_config(&config, reader).expect("builds fine");
        assert_eq!(registry.active(), vec!["cpu"]);
    }

    #[test]
    fn from_config_honours_skip() {
        let tree = crate::sysfs::tests::TempTree::new("registry-from-config-skip");
        tree.file("proc/stat", "cpu  0 0 0 0 0 0 0 0 0 0\n");
        tree.file("proc/meminfo", "MemTotal: 1000 kB\n");
        let reader = tree.reader();

        let mut config = Config::new();
        config.skip = vec!["memory".to_string()];

        let registry = Registry::from_config(&config, reader).expect("builds fine");
        assert_eq!(registry.active(), vec!["cpu"]);
    }
}
