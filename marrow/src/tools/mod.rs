mod caldav;
mod http_fetch;
mod rss;
mod schedule;
mod sl_transit;
mod stathost;

use crate::tool::ToolRegistry;

/// Register all built-in tools with the registry.
pub fn register_all(registry: &mut ToolRegistry) {
    registry.register(rss::RssFeedTool);
    registry.register(http_fetch::HttpFetchTool);
    registry.register(caldav::CalDavCalendarTool);
    registry.register(caldav::CalDavTasksTool);
    registry.register(sl_transit::SlTransitTool);
    registry.register(stathost::StathostTool);
    registry.register(schedule::ScheduleTaskTool);
    registry.register(schedule::ListSchedulesTool);
    registry.register(schedule::RemoveScheduleTool);
}
