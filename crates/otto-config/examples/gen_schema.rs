//! Regenerates `schema/config.json` from [`otto_config::Config`].
//!
//! Run after changing `otto-config/src/schema.rs`:
//! `cargo run -p otto-config --example gen_schema > schema/config.json`

fn main() {
    let schema = schemars::schema_for!(otto_config::Config);
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}
