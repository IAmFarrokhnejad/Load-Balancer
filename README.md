
# Email Domain Validator

A command-line tool to check email domain security records (MX, SPF, DMARC).  
Written in both **Go** and **Rust**, it reads a list of domain names from standard input and outputs a CSV report containing DNS lookup results.

## Features

- **MX Records** – Verify if the domain has mail exchange servers.
- **SPF Records** – Detect Sender Policy Framework (TXT) records.
- **DMARC Records** – Check for DMARC policies under `_dmarc.<domain>`.
- **Concurrent Processing** – Configurable number of workers for fast bulk checks.
- **Timeout Control** – Set a per-lookup timeout to avoid hanging.
- **CSV Output** – Results can be saved to a file or printed to stdout.
- **Error Handling** – Errors are collected per domain and included in the output.

## Usage

Both versions accept the same command-line arguments.

| Flag               | Description                                | Default      |
|--------------------|--------------------------------------------|--------------|
| `-output`, `-o`    | Output CSV file (optional)                 | (stdout)     |
| `-concurrency`, `-c`| Number of concurrent workers               | `10`         |
| `-timeout`, `-t`   | Timeout for DNS lookups (e.g., `5s`, `1m`) | `10s`        |

Domains are read **one per line** from **stdin**.

### Examples

Check a list of domains from `domains.txt` and print results to the console:

```bash
cat domains.txt | ./emailVal
```

Same, but save results to `report.csv` using 20 workers and a 5‑second timeout:

```bash
cat domains.txt | ./emailVal --output report.csv --concurrency 20 --timeout 5s
```

## Go Version

### Build

```bash
go build -o emailVal main.go
```

### Run

```bash
./emailVal < domains.txt
```

### Dependencies

Uses only the standard library (`net`, `context`, `encoding/csv`, etc.). No external dependencies required.

## Rust Version

### Build

Ensure you have [Rust and Cargo](https://rustup.rs/) installed, then:

```bash
cargo build --release
```

The binary will be placed in `target/release/emailVal`.

### Dependencies

Add the following to your `Cargo.toml`:

```toml
[package]
name = "emailVal"
version = "0.1.0"
edition = "2021"

[dependencies]
clap = { version = "4", features = ["derive"] }
csv = "1"
humantime = "2"
tokio = { version = "1", features = ["full"] }
trust-dns-resolver = "0.23"
anyhow = "1"
```

### Run

```bash
./target/release/emailVal < domains.txt
```

## Output Format

The tool outputs a CSV with the following columns:

| Column          | Description                                        |
|-----------------|----------------------------------------------------|
| Domain          | The queried domain name                            |
| Has MX          | `true` if at least one MX record exists            |
| Has SPF         | `true` if a TXT record starting with `v=spf1` found|
| SPF Record      | The full SPF TXT value (if any)                    |
| Has DMARC       | `true` if a DMARC record (`v=DMARC1`) exists       |
| DMARC Record    | The full DMARC TXT value (if any)                  |
| MX Records      | Semi‑colon separated list of MX hosts              |
| Errors          | Semi‑colon separated error messages (if any)       |

## Example

Input file `domains.txt`:

```
example.com
google.com
nonexistent.xyz
```

Command:

```bash
cat domains.txt | ./emailVal -o result.csv
```

Output `result.csv`:

```csv
Domain,Has MX,Has SPF,SPF Record,Has DMARC,DMARC Record,MX Records,Errors
example.com,true,false,,false,,,MX lookup error: no record found
google.com,true,true,v=spf1 include:_spf.google.com ~all,true,v=DMARC1; p=reject; sp=reject; rua=mailto:mailauth-reports@google.com,aspmx.l.google.com;alt1.aspmx.l.google.com;alt2.aspmx.l.google.com;alt3.aspmx.l.google.com;alt4.aspmx.l.google.com,
nonexistent.xyz,false,false,,false,,,MX lookup error: no such host;SPF lookup error: no such host;DMARC lookup error: no such host
```

## License

This project is licensed under the MIT License – see the [LICENSE](LICENSE) file for details.
