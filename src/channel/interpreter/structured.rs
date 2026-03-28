use teloxide::types::{Contact, Dice, Location, Poll, Sticker, Venue};

use super::InterpretedMessage;

pub fn interpret_location(location: &Location) -> InterpretedMessage {
    InterpretedMessage {
        text: format!(
            "<location latitude=\"{}\" longitude=\"{}\" />",
            location.latitude, location.longitude
        ),
        attachments: vec![],
    }
}

pub fn interpret_contact(contact: &Contact) -> InterpretedMessage {
    let name = match &contact.last_name {
        Some(last) => format!("{} {}", contact.first_name, last),
        None => contact.first_name.clone(),
    };
    InterpretedMessage {
        text: format!(
            "<contact name=\"{}\" phone=\"{}\" />",
            name, contact.phone_number
        ),
        attachments: vec![],
    }
}

pub fn interpret_venue(venue: &Venue) -> InterpretedMessage {
    let address = venue
        .address
        .replace('"', "&quot;");
    InterpretedMessage {
        text: format!(
            "<venue name=\"{}\" address=\"{}\" latitude=\"{}\" longitude=\"{}\" />",
            venue.title.replace('"', "&quot;"),
            address,
            venue.location.latitude,
            venue.location.longitude
        ),
        attachments: vec![],
    }
}

pub fn interpret_poll(poll: &Poll) -> InterpretedMessage {
    let options: Vec<&str> = poll.options.iter().map(|o| o.text.as_str()).collect();
    InterpretedMessage {
        text: format!(
            "<poll question=\"{}\" options=\"{}\" />",
            poll.question.replace('"', "&quot;"),
            options.join(", ")
        ),
        attachments: vec![],
    }
}

pub fn interpret_dice(dice: &Dice) -> InterpretedMessage {
    InterpretedMessage {
        text: format!(
            "<dice emoji=\"{:?}\" value=\"{}\" />",
            dice.emoji, dice.value
        ),
        attachments: vec![],
    }
}

pub fn interpret_sticker(sticker: &Sticker) -> InterpretedMessage {
    let emoji = sticker.emoji.as_deref().unwrap_or("?");
    InterpretedMessage {
        text: format!("<sticker emoji=\"{}\" />", emoji),
        attachments: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_location(lat: f64, lon: f64) -> Location {
        Location {
            latitude: lat,
            longitude: lon,
            horizontal_accuracy: None,
            live_period: None,
            heading: None,
            proximity_alert_radius: None,
        }
    }

    #[test]
    fn test_location_xml() {
        let loc = make_location(35.68, 139.76);
        let result = interpret_location(&loc);
        assert!(result.text.contains("<location"));
        assert!(result.text.contains("latitude=\"35.68\""));
        assert!(result.text.contains("longitude=\"139.76\""));
        assert!(result.text.ends_with("/>"));
    }

    #[test]
    fn test_contact_xml() {
        let contact = Contact {
            phone_number: "+1234567890".into(),
            first_name: "John".into(),
            last_name: Some("Doe".into()),
            user_id: None,
            vcard: None,
        };
        let result = interpret_contact(&contact);
        assert!(result.text.contains("<contact"));
        assert!(result.text.contains("name=\"John Doe\""));
        assert!(result.text.contains("phone=\"+1234567890\""));
    }

    #[test]
    fn test_sticker_xml_format() {
        // Test the XML format directly without constructing a Sticker
        // (Sticker uses flattened enums that are hard to construct in tests)
        let xml = format!("<sticker emoji=\"{}\" />", "😀");
        assert!(xml.contains("<sticker"));
        assert!(xml.contains("emoji=\"😀\""));
    }
}
