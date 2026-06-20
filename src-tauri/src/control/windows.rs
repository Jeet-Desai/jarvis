//! Windows accessibility reader — placeholder for the future UI Automation
//! backend. The cross-platform input (enigo) in `mod.rs` already works on
//! Windows; only tree-reading / label-clicking need this platform impl, which
//! will use the `uiautomation` crate (IUIAutomation) to mirror the macOS AX
//! reader. Until then these degrade gracefully with a clear message.

use super::UiElement;
use anyhow::{bail, Result};

pub fn read_ui_elements(_max: usize) -> Result<Vec<UiElement>> {
    bail!("reading the UI tree on Windows isn't implemented yet (planned: UI Automation via the `uiautomation` crate)")
}
// Note: click_ui_element lives in mod.rs now (platform-agnostic: read → click),
// so it works automatically once read_ui_elements is implemented here.
