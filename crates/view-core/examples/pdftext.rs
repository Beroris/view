//! Print the text View finds in PDFs, page by page, with how long each took.
//!
//!     cargo run --release -p view-core --example pdftext -- <file.pdf>... [--quiet]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quiet = args.iter().any(|a| a == "--quiet");
    for path in args.iter().filter(|a| !a.starts_with("--")) {
        let started = std::time::Instant::now();
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                println!("{path}: {e}");
                continue;
            }
        };
        let result = std::panic::catch_unwind(|| {
            let doc = view_core::pdftext::open(data)?;
            let opened = started.elapsed();
            let mut glyphs = 0;
            let mut sample = String::new();
            for i in 0..doc.page_count() {
                let p = doc.page(i).unwrap_or_default();
                glyphs += p.glyphs.len();
                if !quiet {
                    println!("--- page {} ({} characters)\n{}", i + 1, p.glyphs.len(), p.copy(0, p.glyphs.len()));
                } else if sample.len() < 80 {
                    sample.push_str(&p.copy(0, p.glyphs.len().min(80)).replace('\n', " / "));
                }
            }
            Ok::<_, String>(format!("{} pages, {glyphs} characters, opened in {:.0} ms, all text in {:.0} ms  {}", doc.page_count(), opened.as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0, sample.chars().take(80).collect::<String>()))
        });
        match result {
            Ok(Ok(s)) => println!("{path}: {s}"),
            Ok(Err(e)) => println!("{path}: {e}"),
            Err(_) => println!("{path}: PANICKED"),
        }
    }
}
