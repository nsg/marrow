use std::collections::HashMap;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;
use crate::xml;

pub struct RssFeedTool;

impl Tool for RssFeedTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "rss_feed".to_string(),
            description: "Fetches items from an RSS/Atom feed URL and optionally filters by topic"
                .to_string(),
            provides: vec!["rss_feed".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("URL"),
            ParamDef::optional("TOPIC"),
            ParamDef::optional("LIMIT"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "feed_url".to_string(),
            "topic".to_string(),
            "total_items".to_string(),
            "matching_count".to_string(),
            "items".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let url = match params.get("URL") {
                Some(u) if !u.is_empty() => u.clone(),
                _ => {
                    return Ok(serde_json::json!({"error": "missing required parameter: URL"}));
                }
            };

            let topic = params.get("TOPIC").cloned().unwrap_or_default();
            let topic_lower = topic.to_lowercase();
            let limit: usize = params
                .get("LIMIT")
                .and_then(|l| l.parse().ok())
                .unwrap_or(10);

            let resp = ctx.client.get(&url).send().await.map_err(|e| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "HTTP request failed: {e}"
                ))
            })?;

            let status = resp.status().as_u16();
            if status != 200 {
                return Ok(serde_json::json!({
                    "error": "HTTP request failed",
                    "status": status,
                }));
            }

            let body = resp.text().await.map_err(|e| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "failed to read response: {e}"
                ))
            })?;

            let root = match xml::parse(&body) {
                Ok(node) => node,
                Err(e) => {
                    return Ok(serde_json::json!({"error": format!("XML parse failed: {e}")}));
                }
            };

            let all_items = extract_items(&root);
            let total = all_items.len();

            let filtered: Vec<&FeedItem> = if topic_lower.is_empty() {
                all_items.iter().collect()
            } else {
                all_items
                    .iter()
                    .filter(|item| item.matches_topic(&topic_lower))
                    .collect()
            };

            let matching_count = filtered.len();
            let items_json: Vec<serde_json::Value> = filtered
                .into_iter()
                .take(limit)
                .map(|item| {
                    serde_json::json!({
                        "title": item.title,
                        "link": item.link,
                        "pubDate": item.pub_date,
                        "categories": item.categories.join(", "),
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "feed_url": url,
                "topic": topic,
                "total_items": total,
                "matching_count": matching_count,
                "items": items_json,
            }))
        })
    }
}

struct FeedItem {
    title: String,
    link: String,
    pub_date: String,
    description: String,
    categories: Vec<String>,
}

impl FeedItem {
    fn matches_topic(&self, topic_lower: &str) -> bool {
        if self.title.to_lowercase().contains(topic_lower) {
            return true;
        }
        if self.description.to_lowercase().contains(topic_lower) {
            return true;
        }
        self.categories
            .iter()
            .any(|c| c.to_lowercase().contains(topic_lower))
    }
}

fn extract_items(root: &xml::XmlNode) -> Vec<FeedItem> {
    // RSS 2.0: <rss> > <channel> > <item>
    if (root.tag == "rss" || root.tag.ends_with(":rss"))
        && let Some(channel) = root
            .children
            .iter()
            .find(|c| c.tag == "channel" || c.tag.ends_with(":channel"))
    {
        return channel
            .children
            .iter()
            .filter(|c| c.tag == "item" || c.tag.ends_with(":item"))
            .map(parse_rss_item)
            .collect();
    }

    // Atom: <feed> > <entry>
    if root.tag == "feed" || root.tag.ends_with(":feed") || root.tag.contains("Atom") {
        return root
            .children
            .iter()
            .filter(|c| c.tag == "entry" || c.tag.ends_with(":entry"))
            .map(parse_atom_entry)
            .collect();
    }

    // Try looking for channel/item pattern without rss wrapper
    if let Some(channel) = root
        .children
        .iter()
        .find(|c| c.tag == "channel" || c.tag.ends_with(":channel"))
    {
        return channel
            .children
            .iter()
            .filter(|c| c.tag == "item" || c.tag.ends_with(":item"))
            .map(parse_rss_item)
            .collect();
    }

    Vec::new()
}

fn parse_rss_item(node: &xml::XmlNode) -> FeedItem {
    let mut title = String::new();
    let mut link = String::new();
    let mut pub_date = String::new();
    let mut description = String::new();
    let mut categories = Vec::new();

    for child in &node.children {
        let tag = child.tag.rsplit(':').next().unwrap_or(&child.tag);
        let text = child.text.as_deref().unwrap_or("");
        match tag {
            "title" => title = text.to_string(),
            "link" => link = text.to_string(),
            "pubDate" => pub_date = text.to_string(),
            "description" => description = text.to_string(),
            "category" => categories.push(text.to_string()),
            _ => {}
        }
    }

    FeedItem {
        title,
        link,
        pub_date,
        description,
        categories,
    }
}

fn parse_atom_entry(node: &xml::XmlNode) -> FeedItem {
    let mut title = String::new();
    let mut link = String::new();
    let mut pub_date = String::new();
    let mut description = String::new();
    let mut categories = Vec::new();

    for child in &node.children {
        let tag = child.tag.rsplit(':').next().unwrap_or(&child.tag);
        match tag {
            "title" => title = child.text.as_deref().unwrap_or("").to_string(),
            "link" => {
                // Atom links use href attribute
                if let Some(href) = child.attrs.get("href") {
                    link = href.clone();
                } else if let Some(text) = &child.text {
                    link = text.clone();
                }
            }
            "published" | "updated" if pub_date.is_empty() => {
                pub_date = child.text.as_deref().unwrap_or("").to_string();
            }
            "summary" | "content" if description.is_empty() => {
                description = child.text.as_deref().unwrap_or("").to_string();
            }
            "category" => {
                if let Some(term) = child.attrs.get("term") {
                    categories.push(term.clone());
                } else if let Some(text) = &child.text {
                    categories.push(text.clone());
                }
            }
            _ => {}
        }
    }

    FeedItem {
        title,
        link,
        pub_date,
        description,
        categories,
    }
}
