use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

gpui_kit::assets::icon_assets!(ExtraIcons, [Headphones, Smartphone, Link, Unlink, Power]);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn bundles_application_icons_and_default_component_icons() {
        for icon in [
            IconName::Headphones,
            IconName::Smartphone,
            IconName::Settings,
            IconName::Loader,
            IconName::ChevronDown,
            IconName::Copy,
            IconName::Link,
            IconName::Unlink,
            IconName::Power,
        ] {
            assert!(!AppAssets.load(&icon.path()).unwrap().unwrap().is_empty());
        }
    }
}
