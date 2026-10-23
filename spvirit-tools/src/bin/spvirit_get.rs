use argparse::{ArgumentParser, Store, StoreTrue};
use tokio::runtime::Runtime;

use spvirit_tools::spvirit_client::cli::{CommonClientArgs, require_pv_name};
use spvirit_tools::spvirit_client::client::{pvget, pvget_fields};
use spvirit_tools::spvirit_client::format::{OutputFormat, RenderOptions, format_output};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pv_name = String::new();
    let mut raw = false;
    let mut json = false;
    let mut common = CommonClientArgs::new();

    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Minimal PVA pvget client");
        ap.refer(&mut pv_name)
            .add_argument("pv", Store, "PV name to fetch");
        common.add_to_parser(&mut ap);
        ap.refer(&mut raw)
            .add_option(&["--raw"], StoreTrue, "Print raw hex payload");
        ap.refer(&mut json)
            .add_option(&["--json"], StoreTrue, "Print JSON output");
        ap.parse_args_or_exit();
    }

    require_pv_name(&pv_name)?;
    common.init_tracing();
    let fields = common.fields_list();
    let opts = common.into_pv_get_options(pv_name.clone())?;

    let rt = Runtime::new()?;
    let result = rt.block_on(async {
        if fields.is_empty() {
            pvget(&opts).await
        } else {
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            pvget_fields(&opts, &refs).await
        }
    })?;

    let mut render_opts = RenderOptions::default();
    if json {
        render_opts.format = OutputFormat::Json;
    }
    println!(
        "{}",
        format_output(&result.pv_name, &result.value, &render_opts)
    );

    if raw {
        println!("raw_pva: {}", hex::encode(result.raw_pva));
        println!("raw_pvd: {}", hex::encode(result.raw_pvd));
    }

    Ok(())
}
