mod rss;

use crate::tool::ToolRegistry;

/// Register all built-in tools with the registry.
pub fn register_all(registry: &mut ToolRegistry) {
    registry.register(rss::RssFeedTool);
}
