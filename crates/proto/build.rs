fn main() -> Result<(), Box<dyn std::error::Error>> {
    const ROOT: &str = "../../proto";
    println!("cargo:rerun-if-changed={ROOT}");
    // protox compiles in-process, so building needs no system `protoc`.
    let descriptors = protox::compile(
        ["fermah/pay/stellar/v1/buyer.proto", "fermah/pay/stellar/v1/ledger.proto"],
        [ROOT],
    )?;
    tonic_prost_build::configure().compile_fds(descriptors)?;
    Ok(())
}
