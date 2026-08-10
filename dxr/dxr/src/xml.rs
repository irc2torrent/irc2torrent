use quick_xml::de::DeError;
use quick_xml::se::Serializer;

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

/// Custom function for serializing values as XML.
///
/// This function uses a custom [`Serializer`] that expands empty XML elements
/// (for example, `<value><string></string></value>` for an empty string value)
/// instead of using self-closing XML tags, which are not accepted by all XML-RPC
/// implementations.
///
/// This should be a drop-in replacement for [`quick_xml::se::to_string`].
pub fn serialize_xml<T>(value: &T) -> Result<String, DeError>
where
    T: Serialize,
{
    let mut buf = String::new();

    // initialize custom serializer that expands empty elements
    let mut serializer = Serializer::new(&mut buf);
    serializer.expand_empty_elements(true);

    // quick-xml 0.31+ split serialization errors out into their own `SeError`
    // type, which has no `From` impl into `DeError`. The public signature here
    // is kept unchanged -- callers treat this as one opaque XML error -- so the
    // serializer error is folded in via serde's `custom` constructor.
    value.serialize(serializer).map_err(|e| DeError::custom(e.to_string()))?;
    Ok(buf)
}

/// Function for deserializing values from XML.
///
/// This is a wrapper around [`quick_xml::de::from_str`].
pub fn deserialize_xml<'de, T>(string: &'de str) -> Result<T, DeError>
where
    T: Deserialize<'de>,
{
    quick_xml::de::from_str(string)
}
