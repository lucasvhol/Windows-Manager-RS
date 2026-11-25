//! Vendor-neutral readings that Windows itself provides, the same sources Task
//! Manager uses: graphics adapters through the display kernel interface (temperature,
//! fan, power, memory size) and performance counters (GPU load and memory in use, CPU
//! clock). No driver of our own and no administrator rights are needed; values an
//! adapter's driver does not report are left out.

use std::collections::HashMap;

use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTER_PERFDATACAPS, D3DKMT_ADAPTERINFO, D3DKMT_ADAPTERREGISTRYINFO,
    D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_QUERYADAPTERINFO, D3DKMT_SEGMENTSIZEINFO,
    D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_ADAPTERPERFDATA_CAPS, KMTQAITYPE_ADAPTERREGISTRYINFO, KMTQAITYPE_GETSEGMENTSIZE,
    KMTQUERYADAPTERINFOTYPE,
};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::{HSTRING, PCWSTR};

/// One graphics adapter, kept open while the monitor runs.
pub struct Adapter {
    handle: u32,
    /// `luid_0x…_0x…` as performance counter instance names spell it (lower case).
    pub luid: String,
    pub name: String,
    pub dedicated_memory: u64,
    caps: D3DKMT_ADAPTER_PERFDATACAPS,
}

impl Drop for Adapter {
    fn drop(&mut self) {
        let close = D3DKMT_CLOSEADAPTER {
            hAdapter: self.handle,
        };
        let _ = unsafe { D3DKMTCloseAdapter(&close) };
    }
}

fn query<T: Default>(handle: u32, kind: KMTQUERYADAPTERINFOTYPE, mut value: T) -> Option<T> {
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: kind,
        pPrivateDriverData: std::ptr::from_mut(&mut value).cast(),
        PrivateDriverDataSize: size_of::<T>() as u32,
    };
    unsafe { D3DKMTQueryAdapterInfo(&mut q) }.is_ok().then_some(value)
}

/// Length of `luid_0x00000000_0x00000000`.
const LUID_LEN: usize = 26;

pub fn luid_name(high: i32, low: u32) -> String {
    format!("luid_0x{:08x}_0x{low:08x}", high as u32)
}

/// Hardware graphics adapters (the software renderer is left out).
pub fn adapters() -> Vec<Adapter> {
    let mut e = D3DKMT_ENUMADAPTERS2::default();
    if unsafe { D3DKMTEnumAdapters2(&mut e) }.is_err() || e.NumAdapters == 0 {
        return Vec::new();
    }
    let mut infos = vec![D3DKMT_ADAPTERINFO::default(); e.NumAdapters as usize];
    e.pAdapters = infos.as_mut_ptr();
    if unsafe { D3DKMTEnumAdapters2(&mut e) }.is_err() {
        return Vec::new();
    }
    infos.truncate(e.NumAdapters as usize);
    let mut out = Vec::new();
    for info in infos {
        let reg = query(
            info.hAdapter,
            KMTQAITYPE_ADAPTERREGISTRYINFO,
            D3DKMT_ADAPTERREGISTRYINFO::default(),
        );
        let name = reg
            .map(|r| {
                let end = r.AdapterString.iter().position(|&c| c == 0).unwrap_or(260);
                String::from_utf16_lossy(&r.AdapterString[..end])
                    .trim()
                    .to_owned()
            })
            .unwrap_or_default();
        let adapter = Adapter {
            handle: info.hAdapter,
            luid: luid_name(info.AdapterLuid.HighPart, info.AdapterLuid.LowPart),
            dedicated_memory: query(
                info.hAdapter,
                KMTQAITYPE_GETSEGMENTSIZE,
                D3DKMT_SEGMENTSIZEINFO::default(),
            )
            .map_or(0, |s| s.DedicatedVideoMemorySize),
            caps: query(
                info.hAdapter,
                KMTQAITYPE_ADAPTERPERFDATA_CAPS,
                D3DKMT_ADAPTER_PERFDATACAPS::default(),
            )
            .unwrap_or_default(),
            name,
        };
        // Dropping closes the handle of adapters we skip.
        if !adapter.name.is_empty() && !adapter.name.starts_with("Microsoft Basic") {
            out.push(adapter);
        }
    }
    out.sort_by(|a, b| a.luid.cmp(&b.luid));
    out.dedup_by(|a, b| a.luid == b.luid);
    out
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AdapterPerf {
    pub temp_c: Option<f32>,
    pub fan_rpm: Option<f32>,
    /// Power draw as a percentage of the adapter's maximum.
    pub power_pct: Option<f32>,
}

impl Adapter {
    pub fn perf(&self) -> AdapterPerf {
        let Some(p) = query(
            self.handle,
            KMTQAITYPE_ADAPTERPERFDATA,
            D3DKMT_ADAPTER_PERFDATA::default(),
        ) else {
            return AdapterPerf::default();
        };
        AdapterPerf {
            // Deci-degrees Celsius; 0 means the driver does not report it.
            temp_c: (p.Temperature > 0).then(|| p.Temperature as f32 / 10.0),
            fan_rpm: (self.caps.MaxFanRPM > 0).then_some(p.FanRPM as f32),
            // Tenths of a percent.
            power_pct: (p.Power > 0).then(|| p.Power as f32 / 10.0),
        }
    }
}

/// A performance counter query with English counter paths (wildcards allowed).
pub struct Counters {
    query: PDH_HQUERY,
    counters: Vec<Option<PDH_HCOUNTER>>,
}

// The handles are only used from the thread that owns the sampler state.
unsafe impl Send for Counters {}

impl Drop for Counters {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

impl Counters {
    pub fn open(paths: &[&str]) -> Option<Self> {
        let mut query = PDH_HQUERY::default();
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return None;
        }
        let counters = paths
            .iter()
            .map(|p| {
                let mut c = PDH_HCOUNTER::default();
                (unsafe { PdhAddEnglishCounterW(query, &HSTRING::from(*p), 0, &mut c) } == 0).then_some(c)
            })
            .collect();
        let me = Self { query, counters };
        me.collect();
        Some(me)
    }

    /// Takes a new reading; rates are computed between two readings.
    pub fn collect(&self) {
        unsafe { PdhCollectQueryData(self.query) };
    }

    /// Every instance of counter `i`: (instance name, value).
    pub fn values(&self, i: usize) -> Vec<(String, f64)> {
        let Some(Some(c)) = self.counters.get(i) else {
            return Vec::new();
        };
        let (mut size, mut count) = (0u32, 0u32);
        let r = unsafe { PdhGetFormattedCounterArrayW(*c, PDH_FMT_DOUBLE, &mut size, &mut count, None) };
        if r != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        // u64 storage keeps the items aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        if unsafe { PdhGetFormattedCounterArrayW(*c, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) }
            != 0
        {
            return Vec::new();
        }
        (0..count as usize)
            .filter_map(|k| {
                let item = unsafe { &*items.add(k) };
                if item.FmtValue.CStatus > 1 {
                    return None; // PDH_CSTATUS_VALID_DATA = 0, NEW_DATA = 1
                }
                let name = unsafe { item.szName.to_string() }.ok()?;
                Some((name, unsafe { item.FmtValue.Anonymous.doubleValue }))
            })
            .collect()
    }
}

pub const GPU_ENGINE: &str = r"\GPU Engine(*)\Utilization Percentage";
pub const GPU_MEMORY: &str = r"\GPU Adapter Memory(*)\Dedicated Usage";
pub const CPU_PERFORMANCE: &str = r"\Processor Information(_Total)\% Processor Performance";
pub const CPU_FREQUENCY: &str = r"\Processor Information(_Total)\Processor Frequency";

/// GPU load per adapter LUID, as Task Manager shows it: the busiest engine type,
/// each type summed over the processes using it.
pub fn gpu_load(engines: &[(String, f64)]) -> HashMap<String, f32> {
    let mut by_type: HashMap<(String, String), f64> = HashMap::new();
    for (name, v) in engines {
        let lower = name.to_ascii_lowercase();
        let Some(start) = lower.find("luid_") else {
            continue;
        };
        let luid: String = lower[start..].chars().take(LUID_LEN).collect();
        let kind = lower
            .split_once("engtype_")
            .map_or(String::new(), |(_, t)| t.to_owned());
        *by_type.entry((luid, kind)).or_default() += v;
    }
    let mut out: HashMap<String, f32> = HashMap::new();
    for ((luid, _), v) in by_type {
        let e = out.entry(luid).or_default();
        *e = e.max(v.min(100.0) as f32);
    }
    out
}

/// Dedicated memory in use per adapter LUID.
pub fn gpu_memory(instances: &[(String, f64)]) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for (name, v) in instances {
        let lower = name.to_ascii_lowercase();
        if let Some(start) = lower.find("luid_") {
            let luid: String = lower[start..].chars().take(LUID_LEN).collect();
            *out.entry(luid).or_default() += *v as u64;
        }
    }
    out
}

/// Effective CPU clock in MHz from the base frequency and the performance percentage
/// (above 100 % while turbo boosting).
pub fn cpu_mhz(counters: &Counters, perf: usize, freq: usize) -> Option<f32> {
    let p = counters.values(perf).first()?.1;
    let f = counters.values(freq).first()?.1;
    (f > 0.0 && p > 0.0).then(|| (f * p / 100.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_group_by_adapter() {
        assert_eq!(luid_name(0, 0x10c6e), "luid_0x00000000_0x00010c6e");
        let engines = vec![
            (
                "pid_1_luid_0x00000000_0x00010C6E_phys_0_eng_0_engtype_3D".to_owned(),
                30.0,
            ),
            (
                "pid_2_luid_0x00000000_0x00010C6E_phys_0_eng_0_engtype_3D".to_owned(),
                25.0,
            ),
            (
                "pid_2_luid_0x00000000_0x00010C6E_phys_0_eng_3_engtype_VideoDecode".to_owned(),
                40.0,
            ),
            (
                "pid_3_luid_0x00000000_0x00013FC5_phys_0_eng_0_engtype_3D".to_owned(),
                5.0,
            ),
        ];
        let load = gpu_load(&engines);
        assert_eq!(load["luid_0x00000000_0x00010c6e"], 55.0);
        assert_eq!(load["luid_0x00000000_0x00013fc5"], 5.0);
        let mem = gpu_memory(&[("luid_0x00000000_0x00010C6E_phys_0".to_owned(), 1024.0)]);
        assert_eq!(mem["luid_0x00000000_0x00010c6e"], 1024);
    }

    #[test]
    fn real_adapters_and_counters_read() {
        let list = adapters();
        for a in &list {
            println!(
                "{} {} {} MB {:?}",
                a.luid,
                a.name,
                a.dedicated_memory >> 20,
                a.perf()
            );
        }
        let c = Counters::open(&[GPU_ENGINE, GPU_MEMORY, CPU_PERFORMANCE, CPU_FREQUENCY]).expect("pdh");
        std::thread::sleep(std::time::Duration::from_millis(300));
        c.collect();
        let mhz = cpu_mhz(&c, 2, 3);
        println!("cpu {mhz:?} MHz");
        println!("load {:?}", gpu_load(&c.values(0)));
        println!("mem {:?}", gpu_memory(&c.values(1)));
        assert!(mhz.is_some_and(|m| m > 100.0));
    }
}
