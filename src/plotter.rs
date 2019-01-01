extern crate humanize_rs;
extern crate pbr;
extern crate raw_cpuid;
extern crate rayon;
extern crate systemstat;
extern crate num_cpus;

use self::humanize_rs::bytes::Bytes;
use self::pbr::{MultiBar, Units};
use self::raw_cpuid::CpuId;
use chan;
use core_affinity;
#[cfg(feature = "opencl")]
use ocl::gpu_get_info;
use scheduler::create_scheduler_thread;
use std::cmp::{max, min};
use std::path::Path;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use stopwatch::Stopwatch;
use self::systemstat::{System, Platform};
use utils::free_disk_space;
use utils::get_sector_size;
use utils::preallocate;
#[cfg(windows)]
use utils::set_thread_ideal_processor;
use writer::{create_writer_thread, read_resume_info, write_resume_info};

pub const SCOOP_SIZE: u64 = 64;
pub const NUM_SCOOPS: u64 = 4096;
pub const NONCE_SIZE: u64 = SCOOP_SIZE * NUM_SCOOPS;

pub struct Plotter {}

extern "C" {
    pub fn init_shabal() -> ();
    pub fn init_shabal_sse() -> ();
    pub fn init_shabal_avx() -> ();
    pub fn init_shabal_avx2() -> ();
    pub fn init_shabal_avx512() -> ();
}

pub struct PlotterTask {
    pub numeric_id: u64,
    pub start_nonce: u64,
    pub nonces: u64,
    pub output_path: String,
    pub mem: String,
    pub cpu_threads: u8,
    pub gpus: Option<Vec<String>>,
    pub direct_io: bool,
    pub async_io: bool,
    pub quiet: bool,
    pub benchmark: bool,
    pub zcb: bool,
}

pub struct Buffer {
    data: Arc<Mutex<Vec<u8>>>,
}

impl Buffer {
    fn new(buffer_size: usize) -> Self {
        let data = vec![1u8; buffer_size];
        Buffer {
            data: Arc::new(Mutex::new(data)),
        }
    }

    pub fn get_buffer(&self) -> Arc<Mutex<Vec<u8>>> {
        self.data.clone()
    }
}

impl Plotter {
    pub fn new() -> Plotter {
        Plotter {}
    }

    pub fn run(self, mut task: PlotterTask) {
        let sys = System::new();
        let cpuid = CpuId::new();
        let cpu_name = cpuid.get_extended_function_info().unwrap();
        let cpu_name = cpu_name.processor_brand_string().unwrap().trim();
        let cores = num_cpus::get();
        let memory = sys.memory().unwrap();;

        let simd_ext = detect_simd();

        if !task.quiet {
            println!("Engraver {} - PoC2 Plotter\n", crate_version!());
        }

        if !task.quiet && task.benchmark {
            println!("*BENCHMARK MODE*\n");
        }

        if !task.quiet {
            println!(
                "CPU: {} [using {} of {} cores{}{}]",
                cpu_name,
                task.cpu_threads,
                cores,
                if simd_ext != "" { " + " } else { "" },
                simd_ext
            );
        }

        #[cfg(not(feature = "opencl"))]
        let gpu_mem_needed = 0u64;
        #[cfg(feature = "opencl")]
        let gpu_mem_needed = match &task.gpus {
            Some(x) => gpu_get_info(&x, task.quiet),
            None => 0,
        };

        #[cfg(feature = "opencl")]
        let gpu_mem_needed = if task.zcb {
            gpu_mem_needed
        } else {
            gpu_mem_needed / 2
        };

        // use all avaiblable disk space if nonce parameter has been omitted
        let free_disk_space = free_disk_space(&task.output_path);
        if task.nonces == 0 {
            task.nonces = free_disk_space / NONCE_SIZE;
        }

        let gpu = match &task.gpus {
            Some(_) => true,
            None => false,
        };

        // align number of nonces with sector size if direct i/o
        let mut rounded_nonces_to_sector_size = false;
        let mut nonces_per_sector = 1;
        if task.direct_io {
            let sector_size = get_sector_size(&task.output_path);
            nonces_per_sector = sector_size / SCOOP_SIZE;
            if task.nonces % nonces_per_sector > 0 {
                rounded_nonces_to_sector_size = true;
                task.nonces /= nonces_per_sector;
                task.nonces *= nonces_per_sector;
            }
        }

        let plotsize = task.nonces * NONCE_SIZE;

        let file = Path::new(&task.output_path).join(format!(
            "{}_{}_{}",
            task.numeric_id, task.start_nonce, task.nonces
        ));

        if !file.parent().unwrap().exists() {
            println!(
                "Error: specified target path does not exist, path={}",
                &task.output_path
            );
            println!("Shutting down...");
            return;
        }

        // check available disk space
        if free_disk_space < plotsize && !file.exists() && !task.benchmark {
            println!(
                "Error: insufficient disk space, MiB_required={:.2}, MiB_available={:.2}",
                plotsize as f64 / 1024.0 / 1024.0,
                free_disk_space as f64 / 1024.0 / 1024.0
            );
            println!("Shutting down...");
            return;
        }

        // calculate memory usage
        let mem = match calculate_mem_to_use(&task, &memory, nonces_per_sector, gpu, gpu_mem_needed)
        {
            Ok(x) => x,
            Err(_) => return,
        };

        if !task.quiet {
            println!(
                "RAM: Total={:.2} GiB, Free={:.2} GiB, Usage={:.2} GiB",
                memory.total.as_usize() as f64 / 1024.0 / 1024.0 / 1024.0,
                memory.free.as_usize() as f64 / 1024.0 / 1024.0 / 1024.0,
                (mem + gpu_mem_needed) as f64 / 1024.0 / 1024.0 / 1024.0
            );

            #[cfg(feature = "opencl")]
            println!(
                "     HDDcache={:.2} GiB, GPUcache={:.2} GiB,\n",
                mem as f64 / 1024.0 / 1024.0 / 1024.0,
                gpu_mem_needed as f64 / 1024.0 / 1024.0 / 1024.0
            );

            println!("Numeric ID:  {}", task.numeric_id);
            println!("Start Nonce: {}", task.start_nonce);
            println!(
                "Nonces:      {}{}",
                task.nonces,
                if rounded_nonces_to_sector_size {
                    &" (rounded to sector size for fast direct i/o)"
                } else {
                    &""
                }
            );
        }

        if !task.quiet {
            println!("Output File: {}\n", file.display());
        }
        let mut progress = 0;
        if file.exists() {
            if !task.quiet {
                print!("File already exists, reading resume info...");
            }
            let resume_info = read_resume_info(&file);
            match resume_info {
                Ok(x) => progress = x,
                Err(_) => {
                    println!("Error");
                    println!("File is already completed.");
                    println!("Shutting Down...");
                    return;
                }
            }
            if !task.quiet {
                println!("OK");
            }
        } else {
            if !task.quiet {
                print!("Fast file pre-allocation...");
            }
            if !task.benchmark {
                preallocate(&file, plotsize, task.direct_io);
                write_resume_info(&file, 0u64);
            }
            if !task.quiet {
                println!("OK");
            }
        }

        if !task.quiet {
            if progress == 0 {
                println!("Starting plotting...\n");
            } else {
                println!("Resuming plotting from nonce offset {}...\n", progress);
            }
        }

        // determine buffer size
        let num_buffer = if task.async_io { 2 } else { 1 };
        let buffer_size = mem / num_buffer;
        let (tx_empty_buffers, rx_empty_buffers) = chan::bounded(num_buffer as usize);
        let (tx_full_buffers, rx_full_buffers) = chan::bounded(num_buffer as usize);

        for _ in 0..num_buffer {
            let buffer = Buffer::new(buffer_size as usize);
            tx_empty_buffers.send(buffer);
        }

        let mut mb = MultiBar::new();

        let p1x = if !task.quiet {
            let mut p1 = mb.create_bar(plotsize - progress * NONCE_SIZE);
            p1.format("│██░│");
            p1.set_units(Units::Bytes);
            p1.message("Hashing: ");
            p1.show_counter = false;
            p1.set(0);
            Some(p1)
        } else {
            None
        };

        let p2x = if !task.quiet {
            let mut p2 = mb.create_bar(plotsize - progress * NONCE_SIZE);
            p2.format("│██░│");
            p2.set_units(Units::Bytes);
            p2.message("Writing: ");
            p2.show_counter = false;
            p2.set(0);
            Some(p2)
        } else {
            None
        };

        let sw = Stopwatch::start_new();

        unsafe {
            match &*simd_ext {
                "AVX512F" => init_shabal_avx512(),
                "AVX2" => init_shabal_avx2(),
                "AVX" => init_shabal_avx(),
                "SSE2" => init_shabal_sse(),
                _ => init_shabal(),
            }
        }

        let task = Arc::new(task);

        // hi bold! might make this optional in future releases.
        let thread_pinning = true;
        let core_ids = if thread_pinning {
            core_affinity::get_core_ids().unwrap()
        } else {
            Vec::new()
        };

        let hasher = thread::spawn({
            create_scheduler_thread(
                task.clone(),
                rayon::ThreadPoolBuilder::new()
                    .num_threads(task.cpu_threads as usize)
                    .start_handler(move |id| {
                        if thread_pinning {
                            #[cfg(not(windows))]
                            let core_id = core_ids[id % core_ids.len()];
                            #[cfg(not(windows))]
                            core_affinity::set_for_current(core_id);
                            #[cfg(windows)]
                            set_thread_ideal_processor(id % core_ids.len());
                        }
                    }).build()
                    .unwrap(),
                progress,
                p1x,
                rx_empty_buffers.clone(),
                tx_full_buffers.clone(),
                simd_ext,
            )
        });

        let writer = thread::spawn({
            create_writer_thread(
                task.clone(),
                progress,
                p2x,
                rx_full_buffers.clone(),
                tx_empty_buffers.clone(),
            )
        });

        if !task.quiet {
            mb.listen();
        }
        writer.join().unwrap();
        hasher.join().unwrap();

        let elapsed = sw.elapsed_ms() as u64;
        let hours = elapsed / 1000 / 60 / 60;
        let minutes = elapsed / 1000 / 60 - hours * 60;
        let seconds = elapsed / 1000 - hours * 60 * 60 - minutes * 60;

        if !task.quiet {
            println!(
                "\nGenerated {} nonces in {}h{:02}m{:02}s, {:.2} MiB/s, {:.0} nonces/m.",
                task.nonces - progress,
                hours,
                minutes,
                seconds,
                (task.nonces - progress) as f64 * 1000.0 / (elapsed as f64 + 1.0) / 4.0,
                (task.nonces - progress) as f64 * 1000.0 / (elapsed as f64 + 1.0) * 60.0
            );
        }
    }
}

fn calculate_mem_to_use(
    task: &PlotterTask,
    memory: &systemstat::data::Memory,
    nonces_per_sector: u64,
    gpu: bool,
    gpu_mem_needed: u64,
) -> Result<u64, &'static str> {
    let plotsize = task.nonces * NONCE_SIZE;

    let mut mem = match task.mem.parse::<Bytes>() {
        Ok(x) => x.size() as u64,
        Err(_) => {
            println!(
                "Error: Can't parse memory limit parameter, input={}",
                task.mem,
            );
            println!("\nPlease specify a number followed by a unit. If no unit is provided, bytes will be assumed.");
            println!("Supported units: B, KiB, MiB, GiB, TiB, PiB, EiB, KB, MB, GB, TB, PB, EB");
            println!("Example: --mem 10GiB\n");
            println!("Shutting down...");
            return Err("invalid unit");
        }
    };
    if gpu && mem > 0 && mem < gpu_mem_needed + nonces_per_sector * NONCE_SIZE {
        println!("Error: Insufficient host memory for GPU plotting!");
        println!("Shutting down...");
        process::exit(0);
    }

    if gpu && mem > 0 {
        mem -= gpu_mem_needed;
    }

    if mem == 0 {
        mem = plotsize;
    }
    mem = min(mem, plotsize + gpu_mem_needed);

    // opencl requires buffer to be a multiple of 16 (data coalescence magic)
    let nonces_per_sector = if gpu {
        max(16, nonces_per_sector)
    } else {
        nonces_per_sector
    };

    // don't exceed free memory and leave some elbow room 1-1000/1024
    mem = min(mem, (memory.free.as_usize() as u64 - gpu_mem_needed) * 1000 / 1024);

    // rounding single/double buffer
    let num_buffer = if task.async_io { 2 } else { 1 };
    mem /= num_buffer * NONCE_SIZE * nonces_per_sector;
    mem *= num_buffer * NONCE_SIZE * nonces_per_sector;

    // ensure a minimum buffer
    mem = max(mem, num_buffer * NONCE_SIZE * nonces_per_sector);
    Ok(mem)
}

fn detect_simd() -> String {
    if is_x86_feature_detected!("avx512f") {
        String::from("AVX512F")
    } else if is_x86_feature_detected!("avx2") {
        String::from("AVX2")
    } else if is_x86_feature_detected!("avx") {
        String::from("AVX")
    } else if is_x86_feature_detected!("sse2") {
        String::from("SSE2")
    } else {
        String::from("")
    }
}
