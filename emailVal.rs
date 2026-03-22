// Author: Morteza Farrokhnejad
use clap::Parser;
use csv::Writer;
use std::io::{self, BufRead};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::TokioAsyncResolver;

/// Domain information collected from DNS lookups
#[derive(Debug)]
struct DomainInfo {
    domain: String,
    has_mx: bool,
    has_spf: bool,
    spf_record: String,
    has_dmarc: bool,
    dmarc_record: String,
    mx_records: Vec<String>,
    errors: Vec<String>,
}

/// Command line arguments
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Output CSV file (optional)
    #[arg(short, long)]
    output: Option<String>,

    /// Number of concurrent workers
    #[arg(short, long, default_value_t = 10)]
    concurrency: usize,

    /// Timeout for each DNS lookup
    #[arg(short, long, default_value_t = humantime::parse_duration("10s").unwrap())]
    timeout: Duration,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Set up CSV writer (either to file or stdout)
    let mut csv_writer = match args.output {
        Some(path) => {
            let file = std::fs::File::create(path)?;
            Writer::from_writer(file)
        }
        None => Writer::from_writer(io::stdout()),
    };

    // Write CSV header
    csv_writer.write_record(&[
        "Domain",
        "Has MX",
        "Has SPF",
        "SPF Record",
        "Has DMARC",
        "DMARC Record",
        "MX Records",
        "Errors",
    ])?;
    csv_writer.flush()?;

    // Create a shared DNS resolver (clone for each worker)
    let resolver = Arc::new(create_resolver(args.timeout)?);

    // Channels for domain input and results
    let (domain_tx, domain_rx) = mpsc::channel::<String>(args.concurrency);
    let (result_tx, mut result_rx) = mpsc::channel::<DomainInfo>(args.concurrency);

    // Spawn workers
    let mut join_set = JoinSet::new();
    for _ in 0..args.concurrency {
        let mut domain_rx = domain_rx.clone();
        let resolver = resolver.clone();
        let result_tx = result_tx.clone();
        join_set.spawn(async move {
            while let Some(domain) = domain_rx.recv().await {
                let info = check_domain(&domain, &resolver, args.timeout).await;
                // Ignore send errors (if receiver is dropped)
                let _ = result_tx.send(info).await;
            }
        });
    }
    // Drop the original sender to close the channel when input is exhausted
    drop(domain_tx);

    // Read domains from stdin and send to workers
    let stdin = io::stdin();
    let reader = stdin.lock();
    let mut lines = reader.lines();
    let domain_tx = domain_tx; // move into the async block
    let handle = tokio::spawn(async move {
        while let Some(Ok(domain)) = lines.next() {
            let domain = domain.trim().to_string();
            if domain.is_empty() {
                continue;
            }
            if domain_tx.send(domain).await.is_err() {
                break; // receiver closed
            }
        }
    });

    // Wait for all workers to finish (after input is exhausted)
    while let Some(res) = join_set.join_next().await {
        res?;
    }
    // Close results channel
    drop(result_tx);

    // Write results as they arrive
    while let Some(info) = result_rx.recv().await {
        write_result(&mut csv_writer, &info)?;
    }

    // Ensure the stdin reading task has completed
    handle.await?;

    Ok(())
}

/// Create a DNS resolver with a custom timeout
fn create_resolver(timeout: Duration) -> anyhow::Result<TokioAsyncResolver> {
    let mut opts = ResolverOpts::default();
    opts.timeout = timeout;
    // Use Google DNS as a default, but you can change this
    let resolver = TokioAsyncResolver::tokio(ResolverConfig::google(), opts)?;
    Ok(resolver)
}

/// Perform all DNS checks for a single domain
async fn check_domain(domain: &str, resolver: &TokioAsyncResolver, timeout: Duration) -> DomainInfo {
    let mut info = DomainInfo {
        domain: domain.to_string(),
        has_mx: false,
        has_spf: false,
        spf_record: String::new(),
        has_dmarc: false,
        dmarc_record: String::new(),
        mx_records: Vec::new(),
        errors: Vec::new(),
    };

    // MX lookup
    match timeout(timeout, resolver.mx_lookup(domain)).await {
        Ok(Ok(lookup)) => {
            info.has_mx = !lookup.is_empty();
            for mx in lookup.iter() {
                info.mx_records.push(mx.host().to_string());
            }
        }
        Ok(Err(e)) => info.errors.push(format!("MX lookup error: {}", e)),
        Err(_) => info.errors.push("MX lookup timeout".to_string()),
    }

    // SPF lookup (TXT records)
    match timeout(timeout, resolver.txt_lookup(domain)).await {
        Ok(Ok(lookup)) => {
            for record in lookup.iter() {
                let txt_data = record
                    .data()
                    .and_then(|data| data.as_txt())
                    .map(|txt| txt.txt_data().concat())
                    .unwrap_or_default();
                if txt_data.starts_with("v=spf1") {
                    info.has_spf = true;
                    info.spf_record = txt_data;
                    break;
                }
            }
        }
        Ok(Err(e)) => info.errors.push(format!("SPF lookup error: {}", e)),
        Err(_) => info.errors.push("SPF lookup timeout".to_string()),
    }

    // DMARC lookup (_dmarc.domain)
    let dmarc_domain = format!("_dmarc.{}", domain);
    match timeout(timeout, resolver.txt_lookup(&dmarc_domain)).await {
        Ok(Ok(lookup)) => {
            for record in lookup.iter() {
                let txt_data = record
                    .data()
                    .and_then(|data| data.as_txt())
                    .map(|txt| txt.txt_data().concat())
                    .unwrap_or_default();
                if txt_data.starts_with("v=DMARC1") {
                    info.has_dmarc = true;
                    info.dmarc_record = txt_data;
                    break;
                }
            }
        }
        Ok(Err(e)) => info.errors.push(format!("DMARC lookup error: {}", e)),
        Err(_) => info.errors.push("DMARC lookup timeout".to_string()),
    }

    info
}

/// Write a single DomainInfo to the CSV writer
fn write_result(writer: &mut Writer<impl io::Write>, info: &DomainInfo) -> anyhow::Result<()> {
    writer.write_record(&[
        &info.domain,
        &info.has_mx.to_string(),
        &info.has_spf.to_string(),
        &info.spf_record,
        &info.has_dmarc.to_string(),
        &info.dmarc_record,
        &info.mx_records.join(";"),
        &info.errors.join(";"),
    ])?;
    writer.flush()?;
    Ok(())
}