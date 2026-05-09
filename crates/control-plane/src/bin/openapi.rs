//! Emit the control-plane OpenAPI document as JSON.
//!
//! Usage:
//! ```sh
//! cargo run -p control-plane --bin nanovm-openapi > docs/openapi.json
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use control_plane::openapi_spec;

fn main() {
    let spec = openapi_spec();
    println!(
        "{}",
        serde_json::to_string_pretty(&spec).expect("openapi spec serialization must succeed")
    );
}
