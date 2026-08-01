use std::{any::Any, sync::Arc};

/// A wrapper around `dyn Any`, used for passing opaque user data through egui.
///
/// For example, this is used by [`crate::ViewportCommand::Screenshot`] and
/// [`crate::PlatformOutput::presentation_token`]. Egui preserves the value but does not
/// interpret it.
#[derive(Clone, Debug, Default)]
pub struct UserData {
    /// The opaque user value.
    pub data: Option<Arc<dyn Any + Send + Sync>>,
}

impl UserData {
    /// You can also use [`Self::default`].
    pub fn new(user_info: impl Any + Send + Sync) -> Self {
        Self {
            data: Some(Arc::new(user_info)),
        }
    }

    /// Returns the stored value when it has type `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.data.as_deref()?.downcast_ref()
    }
}

impl PartialEq for UserData {
    fn eq(&self, other: &Self) -> bool {
        match (&self.data, &other.data) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for UserData {}

impl std::hash::Hash for UserData {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.data.as_ref().map(Arc::as_ptr).hash(state);
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for UserData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_none() // can't serialize an `Any`
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for UserData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UserDataVisitor;

        impl serde::de::Visitor<'_> for UserDataVisitor {
            type Value = UserData;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a None value")
            }

            fn visit_none<E>(self) -> Result<UserData, E>
            where
                E: serde::de::Error,
            {
                Ok(UserData::default())
            }
        }

        deserializer.deserialize_option(UserDataVisitor)
    }
}
