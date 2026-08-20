//! What one rescan of the user colour table folder costs.
//!
//! ```text
//! cargo run --release -p color_tables --example user_table_scan_cost
//! ```
//!
//! `UserTableLibrary::refresh` runs on the UI thread on every window focus
//! regain, so its cost is a user-visible property and not an implementation
//! detail: before the listing short circuit and the read cap, a folder with
//! one stray 50 MB `.txt` in it froze the window for roughly a fifth of a
//! second on every alt-tab. This is the measurement that says so, kept so
//! the claim can be re-checked rather than believed.
//!
//! Four things are timed:
//!
//! * `open` - a cold library, which parses everything, at 1/10/50/200 small
//!   palettes;
//! * `refresh` - an already-scanned library asked again, which is exactly
//!   what one alt-tab costs;
//! * the worst folder both caps allow - twenty files that are each just
//!   under the per-file cap, which is what the per-scan budget is for;
//! * `palette_offers_with_user_tables` - what a combo popup rebuilds per
//!   frame while it is open, at several table sizes.
//!
//! It writes tens of megabytes into the system temp directory while it runs
//! and cleans up after itself.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use color_tables::user::{UserTableLibrary, palette_offers_with_user_tables};
use color_tables::{ColorTableFamily, ColorTableSet};

const SMALL_PAL: &str = "\
Product: BV
Units: KTS
Color: -120 130   0 130   200   0 200
Color:  -60 200   0 200    60 220 220
Color:  -20  60 220 220     8  60  70
Color:   -1   8  60  70     8  60  70
Color:    1  70  20  20   220  60  60
Color:   20 220  60  60   255 230  60
Color:   60 255 230  60   255 255 255
";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("scan-cost").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create");
    dir
}

fn fill(dir: &Path, count: usize) {
    for index in 0..count {
        std::fs::write(dir.join(format!("Palette {index:03}.pal")), SMALL_PAL).expect("write");
    }
}

/// Back-date every stamp in the folder by a minute, so the fixture looks
/// like a folder rather than like something written a microsecond ago.
///
/// A scan does not trust a stamp from its own moment: a file saved inside
/// the clock's current step could have been saved twice inside it, so the
/// next scan re-reads it instead of believing the listing. That guard is
/// about correctness rather than cost, and it fires on files this program
/// wrote a millisecond before measuring - whereas the folder an analyst
/// alt-tabs back to was last touched seconds, or weeks, ago. Without this
/// the "refresh" column would measure the guard and not the short circuit.
fn age(dir: &Path) {
    let when = SystemTime::now() - Duration::from_secs(60);
    for entry in std::fs::read_dir(dir).expect("list") {
        let path = entry.expect("entry").path();
        if let Ok(file) = std::fs::File::options().write(true).open(&path) {
            let _ = file.set_modified(when);
        }
    }
}

/// A palette of about `len` bytes made of real `Color:` rows.
///
/// Padding with one enormous comment would measure the wrong thing: the cost
/// of a scan is dominated by parsing rows, not by reading bytes, so a
/// fixture that is 2 MB of comment reads as ten times cheaper than a 2 MB
/// palette anybody would actually have. This is roughly 75,000 rows per
/// megabyte, which is what a large exported palette looks like.
fn dense_palette(len: usize) -> String {
    let mut text = String::from("Product: BV\nUnits: KTS\n");
    let mut index = 0usize;
    while text.len() < len {
        let value = index as f64 * 0.001 - 100.0;
        text.push_str(&format!(
            "Color: {value:.3} {} {} {}\n",
            index % 255,
            (index / 3) % 255,
            (index / 7) % 255
        ));
        index += 1;
    }
    text
}

/// Best of `runs` full library opens (a cold library: parses everything).
fn time_open(dir: &Path, runs: u32) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        let library = UserTableLibrary::open(dir);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(library.tables().len());
        best = best.min(elapsed);
    }
    best
}

/// Best of `runs` refreshes of an ALREADY-scanned library - this is what one
/// alt-tab costs.
fn time_refresh(dir: &Path, runs: u32) -> f64 {
    let mut library = UserTableLibrary::open(dir);
    let mut best = f64::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        library.refresh();
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(library.tables().len());
        best = best.min(elapsed);
    }
    best
}

fn main() {
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("scan-cost"));
    // Warm the page cache and the allocator.
    let warm = scratch("warm");
    fill(&warm, 10);
    let _ = time_open(&warm, 3);

    println!("--- focus rescan: an already-open library, refresh() ---");
    for count in [1usize, 10, 50, 200] {
        let dir = scratch(&format!("n{count}"));
        fill(&dir, count);
        age(&dir);
        println!(
            "{count:>4} small .pal  open {:7.2} ms   refresh {:7.2} ms",
            time_open(&dir, 5),
            time_refresh(&dir, 5)
        );
    }

    // The headline case: 10 real palettes plus a 50 MB stray .txt, which is
    // what `.txt` being an accepted extension makes plausible.
    let dir = scratch("stray");
    fill(&dir, 10);
    let stray = "these are notes, not a palette\n".repeat(50 * 1024 * 1024 / 30);
    std::fs::write(dir.join("notes.txt"), &stray).expect("write stray");
    age(&dir);
    println!(
        "  10 small .pal + one {:.1} MB notes.txt  open {:7.2} ms   refresh {:7.2} ms",
        stray.len() as f64 / (1024.0 * 1024.0),
        time_open(&dir, 5),
        time_refresh(&dir, 5)
    );
    let library = UserTableLibrary::open(&dir);
    println!(
        "     -> {} tables, {} faults{}",
        library.tables().len(),
        library.faults().len(),
        library
            .faults()
            .first()
            .map(|fault| format!(" ({fault})"))
            .unwrap_or_default()
    );

    // The worst folder both caps allow: twenty files that are each just
    // under the per-file cap, so the per-file cap alone lets through forty
    // megabytes of reading and parsing on the UI thread. The per-scan budget
    // is what stops that, and the files past it are faults rather than
    // silence.
    println!("--- the worst legal folder: per-scan budget ---");
    let dir = scratch("budget");
    let big = dense_palette(2 * 1024 * 1024 - 4096);
    for index in 0..20 {
        std::fs::write(dir.join(format!("Big {index:02}.pal")), &big).expect("write big");
    }
    age(&dir);
    let open = time_open(&dir, 3);
    let library = UserTableLibrary::open(&dir);
    println!(
        "  20 x {:.1} MB of real rows, each just legal  open {open:7.2} ms   refresh {:7.2} ms",
        big.len() as f64 / (1024.0 * 1024.0),
        time_refresh(&dir, 5)
    );
    println!(
        "     -> {} tables, {} faults{}",
        library.tables().len(),
        library.faults().len(),
        library
            .faults()
            .first()
            .map(|fault| format!(" ({fault})"))
            .unwrap_or_default()
    );

    // The per-frame picker cost: a big-but-legal table, rebuilt 10 times.
    println!("--- palette_offers_with_user_tables, 10 rebuilds ---");
    for stops in [1000usize, 20_000, 84_000, 843_305] {
        let dir = scratch(&format!("big{stops}"));
        let mut text = String::from("Product: BV\nUnits: KTS\n");
        for index in 0..stops {
            let value = index as f64 * 0.001 - 100.0;
            text.push_str(&format!(
                "Color: {value:.3} {} {} {}\n",
                index % 255,
                (index / 3) % 255,
                (index / 7) % 255
            ));
        }
        std::fs::write(dir.join("Big.pal"), &text).expect("write big");
        let library = UserTableLibrary::open(&dir);
        let installed = ColorTableSet::default()
            .for_family(ColorTableFamily::Velocity)
            .clone();
        let loaded = library.tables().len();
        let start = Instant::now();
        for _ in 0..10 {
            let offers =
                palette_offers_with_user_tables(ColorTableFamily::Velocity, &installed, &library);
            std::hint::black_box(offers.len());
        }
        println!(
            "{stops:>7} stops ({:.1} MB, {loaded} loaded)  {:7.2} ms",
            text.len() as f64 / (1024.0 * 1024.0),
            start.elapsed().as_secs_f64() * 1000.0
        );
    }

    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("scan-cost"));
}
