use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{NsReader, Writer};
use std::collections::HashMap;
use std::io::Cursor;

/// An XML element represented as a plain data structure for easy
/// conversion to/from Lua tables.
#[derive(Debug, Clone)]
pub struct XmlNode {
    pub tag: String,
    pub attrs: HashMap<String, String>,
    pub text: Option<String>,
    pub children: Vec<XmlNode>,
}

/// Parse an XML string into a tree of `XmlNode`s.
/// Namespace URIs are resolved and prefixed to tag names (e.g. `DAV::displayname`).
pub fn parse(input: &str) -> Result<XmlNode, String> {
    let mut reader = NsReader::from_str(input);
    let mut stack: Vec<XmlNode> = Vec::new();

    loop {
        match reader.read_resolved_event() {
            Ok((ns, Event::Start(ref e))) => {
                let node = element_to_node(ns, e)?;
                stack.push(node);
            }
            Ok((ns, Event::Empty(ref e))) => {
                let node = element_to_node(ns, e)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => return Ok(node),
                }
            }
            Ok((_, Event::Text(ref e))) => {
                let text = e
                    .unescape()
                    .map_err(|err| format!("text unescape error: {err}"))?
                    .to_string();
                let trimmed = text.trim();
                if !trimmed.is_empty()
                    && let Some(current) = stack.last_mut()
                {
                    match &mut current.text {
                        Some(existing) => existing.push_str(trimmed),
                        None => current.text = Some(trimmed.to_string()),
                    }
                }
            }
            Ok((_, Event::End(_))) => {
                let finished = stack.pop().ok_or("unexpected closing tag")?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(finished),
                    None => return Ok(finished),
                }
            }
            Ok((_, Event::Decl(_) | Event::Comment(_) | Event::PI(_))) => {}
            Ok((_, Event::Eof)) => {
                return match stack.into_iter().next() {
                    Some(root) => Ok(root),
                    None => Err("empty XML document".to_string()),
                };
            }
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
    }
}

/// Encode an `XmlNode` tree back to an XML string.
pub fn encode(node: &XmlNode) -> Result<String, String> {
    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

    write_node(&mut writer, node)?;

    let buf = writer.into_inner().into_inner();
    String::from_utf8(buf).map_err(|e| format!("UTF-8 error: {e}"))
}

fn element_to_node(ns: quick_xml::name::ResolveResult, e: &BytesStart) -> Result<XmlNode, String> {
    let local = e.local_name();
    let local_name =
        std::str::from_utf8(local.as_ref()).map_err(|err| format!("tag decode error: {err}"))?;

    let tag = if let quick_xml::name::ResolveResult::Bound(ns) = ns {
        let ns_str = std::str::from_utf8(ns.as_ref())
            .map_err(|err| format!("namespace decode error: {err}"))?;
        format!("{ns_str}:{local_name}")
    } else {
        local_name.to_string()
    };

    let mut attrs = HashMap::new();
    for attr in e.attributes().flatten() {
        let key = std::str::from_utf8(attr.key.as_ref())
            .map_err(|err| format!("attr key decode error: {err}"))?
            .to_string();
        let val = attr
            .unescape_value()
            .map_err(|err| format!("attr value decode error: {err}"))?
            .to_string();
        attrs.insert(key, val);
    }

    Ok(XmlNode {
        tag,
        attrs,
        text: None,
        children: Vec::new(),
    })
}

fn write_node(writer: &mut Writer<Cursor<Vec<u8>>>, node: &XmlNode) -> Result<(), String> {
    let mut start = BytesStart::new(&node.tag);
    for (k, v) in &node.attrs {
        start.push_attribute((k.as_str(), v.as_str()));
    }

    let has_content = node.text.is_some() || !node.children.is_empty();

    if !has_content {
        writer
            .write_event(Event::Empty(start))
            .map_err(|e| format!("XML write error: {e}"))?;
    } else {
        writer
            .write_event(Event::Start(start))
            .map_err(|e| format!("XML write error: {e}"))?;

        if let Some(ref text) = node.text {
            writer
                .write_event(Event::Text(BytesText::new(text)))
                .map_err(|e| format!("XML write error: {e}"))?;
        }

        for child in &node.children {
            write_node(writer, child)?;
        }

        writer
            .write_event(Event::End(BytesEnd::new(&node.tag)))
            .map_err(|e| format!("XML write error: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        let xml = r#"<root><child attr="val">hello</child></root>"#;
        let node = parse(xml).unwrap();
        assert_eq!(node.tag, "root");
        assert_eq!(node.children.len(), 1);
        assert_eq!(node.children[0].tag, "child");
        assert_eq!(node.children[0].text.as_deref(), Some("hello"));
        assert_eq!(node.children[0].attrs.get("attr").unwrap(), "val");

        let encoded = encode(&node).unwrap();
        let reparsed = parse(&encoded).unwrap();
        assert_eq!(reparsed.children[0].text.as_deref(), Some("hello"));
    }

    #[test]
    fn empty_element() {
        let xml = r#"<root><empty/></root>"#;
        let node = parse(xml).unwrap();
        assert_eq!(node.children.len(), 1);
        assert_eq!(node.children[0].tag, "empty");
        assert!(node.children[0].children.is_empty());
        assert!(node.children[0].text.is_none());
    }

    #[test]
    fn caldav_propfind() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/calendars/stefan/personal/</href>
    <propstat>
      <prop>
        <displayname>Personal</displayname>
      </prop>
    </propstat>
  </response>
</multistatus>"#;
        let node = parse(xml).unwrap();
        assert_eq!(node.tag, "DAV::multistatus");
        let response = &node.children[0];
        assert_eq!(response.tag, "DAV::response");
        let href = &response.children[0];
        assert_eq!(href.text.as_deref(), Some("/calendars/stefan/personal/"));
    }
}
