// Modified for Distill by Samuel Fajreldines, 2026.
//! JSON stdin/stdout bridge for the offline production retrieval benchmark.
use std::io::{Read, Write};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().take(8 * 1024 * 1024 + 1).read_to_string(&mut input)?;
    anyhow::ensure!(input.len() <= 8 * 1024 * 1024, "adapter request exceeds 8 MiB");
    let request = serde_json::from_str(&input)?;
    let response = distill_shell::stored_output_ask::benchmark_response(request).await?;
    serde_json::to_writer(std::io::stdout().lock(), &response)?;
    std::io::stdout().write_all(b"\n")?;
    Ok(())
}
