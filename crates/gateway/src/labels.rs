//! Enums whose variants are metric label values.

/// Defines a fieldless enum, a method naming each variant's label, and
/// `ALL`, every variant: [`crate::telemetry`] registers the counters alerts
/// watch at zero for each, so a variant added here is registered with the
/// rest.
macro_rules! labels {
    (
        $(#[$meta:meta])*
        pub enum $name:ident as $method:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $label:literal, )*
        }
    ) => {
        $(#[$meta])*
        pub enum $name {
            $( $(#[$vmeta])* $variant, )*
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            #[must_use]
            pub const fn $method(self) -> &'static str {
                match self {
                    $(Self::$variant => $label,)*
                }
            }
        }
    };
}

pub(crate) use labels;
