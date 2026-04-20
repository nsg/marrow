mod caldav;
mod http_fetch;
mod rss;

use crate::tool::ToolRegistry;

/// Register all built-in tools with the registry.
pub fn register_all(registry: &mut ToolRegistry) {
    registry.register(rss::RssFeedTool);
    registry.register(http_fetch::HttpFetchTool);
    registry.register(caldav::CalDavCalendarTool);
    registry.register(caldav::CalDavTasksTool);
}
