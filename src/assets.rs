//! Application asset source: serves the Rymd logo and defers everything
//! else to gpui-component's bundled icons.

use gpui::{AssetSource, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match path {
            "rymd.svg" => Ok(Some(std::borrow::Cow::Borrowed(include_bytes!(
                "../assets/rymd.svg"
            )))),
            _ => gpui_component_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>, anyhow::Error> {
        let mut out = gpui_component_assets::Assets.list(path)?;
        out.push("rymd.svg".into());
        Ok(out)
    }
}
