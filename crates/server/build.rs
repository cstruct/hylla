//! Rebuilds hylla when the control plane UI's hydration bundle changes, such as when `mise
//! run ui` builds it, since the bundle is embedded only if it's there when hylla compiles.

fn main() {
    println!("cargo:rerun-if-changed=../../target/site/pkg");
}
