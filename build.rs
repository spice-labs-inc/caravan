fn main() {
    // If CARAVAN_VERSION is set (by CI from the git tag), bake it in.
    // Otherwise cargo rerun-if-env-changed ensures rebuilds when it changes.
    println!("cargo:rerun-if-env-changed=CARAVAN_VERSION");
}
