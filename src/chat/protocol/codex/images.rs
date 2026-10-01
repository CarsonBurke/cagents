//! Structured image output stays out of the bounded tool text disclosure.
use super::*;

pub(super) fn tool_result(
    id: &str,
    status: ToolStatus,
    name: String,
    input: String,
    result: &Value,
) -> Vec<Item> {
    let mut images = Vec::new();
    let output = without_images(result, &mut images);
    let mut items = vec![Item {
        id: id.into(),
        role: Role::Assistant,
        at: None,
        content: ItemContent::Tool {
            name,
            input,
            output: if result.is_null() {
                String::new()
            } else {
                output.to_string()
            },
            status,
        },
    }];
    items.extend(images.into_iter().enumerate().map(|(index, path)| Item {
        id: format!("{id}/image/{index}"),
        role: Role::Assistant,
        at: None,
        content: ItemContent::Image {
            path,
            description: "Tool image".into(),
        },
    }));
    items
}

fn without_images(value: &Value, images: &mut Vec<String>) -> Value {
    let image = match value["type"].as_str() {
        Some("inputImage" | "input_image") => value["imageUrl"]
            .as_str()
            .or_else(|| value["image_url"].as_str())
            .filter(|path| path.len() <= chat::MAX_INLINE_IMAGE_URL_BYTES)
            .map(str::to_owned),
        Some("image") => value["data"]
            .as_str()
            .zip(value["mimeType"].as_str())
            .filter(|(data, mime)| {
                data.len() <= chat::MAX_INLINE_IMAGE_URL_BYTES
                    && matches!(
                        *mime,
                        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                    )
            })
            .map(|(data, mime)| format!("data:{mime};base64,{data}")),
        _ => None,
    };
    if matches!(
        value["type"].as_str(),
        Some("inputImage" | "input_image" | "image")
    ) {
        let accepted = image.filter(|path| path.len() <= chat::MAX_INLINE_IMAGE_URL_BYTES);
        if let Some(path) = accepted {
            images.push(path);
            return json!({"type":"text","text":"[Image shown in the conversation]"});
        }
        return json!({"type":"text","text":"[Image unavailable or exceeds the 16 MiB image limit]"});
    }
    match value {
        Value::Array(parts) => Value::Array(
            parts
                .iter()
                .map(|part| without_images(part, images))
                .collect(),
        ),
        Value::Object(object) if object.contains_key("content") => {
            let result = object
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        if key == "content" {
                            without_images(value, images)
                        } else {
                            value.clone()
                        },
                    )
                })
                .collect();
            Value::Object(result)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_images_survive_the_text_bound_and_preserve_text_and_status() {
        let image = format!("data:image/png;base64,{}", "QUJD".repeat(200_000));
        for result in [
            json!([{"type":"inputText","text":"Three alternatives"},
                {"type":"inputImage","imageUrl":image}, {"type":"input_image","image_url":image}]),
            json!({"content":[{"type":"text","text":"Three alternatives"},
                {"type":"image","mimeType":"image/png","data":"QUJD".repeat(200_000)}]}),
        ] {
            let kind = if result.is_array() {
                "dynamicToolCall"
            } else {
                "mcpToolCall"
            };
            let mut item = json!({"id":"call","type":kind,"status":"completed", "tool":"make", "server":"images", "arguments":{}});
            let field = if result.is_array() {
                "contentItems"
            } else {
                "result"
            };
            item[field] = result;
            let items = parse_item(&item, true, "turn").unwrap();
            assert!(
                matches!(&items[0].content, ItemContent::Tool { output, status: ToolStatus::Succeeded, .. }
                if output.contains("Three alternatives") && !output.contains("QUJD"))
            );
            assert!(items.len() >= 2);
            assert!(
                matches!(&items[1].content, ItemContent::Image { path, .. } if path.len() > MAX_ITEM_BYTES)
            );
            assert_eq!(items[1].id, "turn/call/image/0");
        }
    }

    #[test]
    fn generated_images_use_saved_files_or_inline_payloads() {
        for (field, value, expected) in [
            ("savedPath", "/fixture/image.png", "/fixture/image.png"),
            ("result", "QUJD", "data:image/png;base64,QUJD"),
        ] {
            let mut item =
                json!({"id":"generated", "type":"imageGeneration", "status":"completed"});
            item[field] = json!(value);
            let items = parse_item(&item, true, "turn").unwrap();
            assert!(
                matches!(&items[0].content, ItemContent::Image { path, .. } if path == expected)
            );
        }
    }
}
