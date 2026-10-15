use anyhow::Result;
use installrs::Installer;

pub fn install(i: &mut Installer) -> Result<()> {
    let out = std::env::var("INSTALLRS_TEST_OUT")
        .expect("INSTALLRS_TEST_OUT must be set before running the test installer");
    i.set_out_dir(&out);
    i.dir(installrs::source!("pack", external = true), "pack").install()?;
    i.file(installrs::source!("readme.txt", external = true), "readme.txt")
        .install()?;
    let (done, total) = i.bytes_progress();
    if done != total {
        anyhow::bail!("installed {done} of {total} bytes");
    }
    Ok(())
}

pub fn uninstall(_i: &mut Installer) -> Result<()> {
    Ok(())
}
