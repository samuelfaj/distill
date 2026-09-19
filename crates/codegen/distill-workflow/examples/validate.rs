// Modified for Distill by Samuel Fajreldines, 2026.
use std::io::Read as _;

fn main() {
    let mut script = String::new();
    std::io::stdin()
        .read_to_string(&mut script)
        .expect("read stdin");

    match distill_workflow::validate_script(&script, None) {
        Ok(report) => {
            println!("META OK: name={} phases={}", report.name, report.phases);
            println!("RUN OK: {}", report.outcome_summary);
        }
        Err(distill_workflow::ValidationError::Meta(e)) => {
            println!("META FAIL: {e}");
            std::process::exit(1);
        }
        Err(distill_workflow::ValidationError::Run(e)) => {
            println!("RUN FAIL: {e}");
            std::process::exit(2);
        }
    }
}
