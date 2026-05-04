mod caldav;
mod http_fetch;
mod memory_delete;
mod memory_search;
mod memory_update;
mod rss;
mod schedule;
mod sl_transit;
mod state_get;
mod state_set;
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
    registry.register(memory_update::MemoryUpdateTool);
    registry.register(memory_delete::MemoryDeleteTool);
    registry.register(memory_search::MemorySearchTool);
    registry.register(state_get::StateGetTool);
    registry.register(state_set::StateSetTool);
}
