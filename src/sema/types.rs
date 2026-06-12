//! Leaf type-mapping helpers that do not need the cross-file symbol table.
//!
//! The recursive mapper (which resolves user/native types and applies namespaces)
//! lives in [`super::Program`]; this module handles the parts that are purely
//! local: primitives, the fixed-width integer aliases, and the container heads.

/// Map a Haxe primitive to its C++98 spelling.
/// Returns `None` for anything that is not a built-in scalar type.
///
/// Note: `Dynamic`/`Any` are deliberately *not* mapped here. Only the empty
/// structure `{}` (an `Anon` with no fields) is erased to `void*`; `Dynamic`/`Any`
/// is left unmapped so it can serve as the overload marker on `@:overload`'d native
/// methods — a call's concrete type is resolved from the matching overload, not from
/// a fixed `void*` spelling.
pub fn map_primitive(name: &str) -> Option<&'static str> {
    Some(match name {
        "Int" => "int",
        // Haxe `Float` is a 64-bit double on every official target; C `float`
        // would silently halve the precision.
        "Float" => "double",
        // Genuine single-precision: the standard `Single`, and hxcpp's
        // `cpp.Float32`/`cpp.Float64` (matched by their last path segment).
        "Single" => "float",
        "Float32" => "float",
        "Float64" => "double",
        "Bool" => "bool",
        "Void" => "void",
        "String" => "std::string",
        // `StringBuf` lowers to a `std::string` accumulator: `new StringBuf()` →
        // `std::string()`, `add`/`addChar` → `+=`, `toString()` → the string itself.
        "StringBuf" => "std::string",
        "UInt8" => "uint8_t",
        "UInt16" => "uint16_t",
        "UInt32" => "uint32_t",
        "UInt" => "unsigned int",
        _ => return None,
    })
}

/// The fixed-width unsigned aliases (`UInt8`/`UInt16`/`UInt32`). These are
/// commonly declared as `typedef UInt8 = UInt;` purely to keep the Haxe valid;
/// Hatchet maps them directly to `<stdint.h>` types and never emits the typedef.
pub fn is_uint_shim(name: &str) -> bool {
    matches!(name, "UInt8" | "UInt16" | "UInt32")
}

/// Whether an `enum abstract`'s underlying type is integral — i.e. lowers to a C++
/// `enum` (`Int`/`UInt`/`UInt8/16/32`). A non-integral backing (`String`/`Float`)
/// lowers instead to a namespace of typed `static const` constants.
pub fn is_integral_underlying(ty: &crate::ast::Type) -> bool {
    matches!(
        ty,
        crate::ast::Type::Named { path, .. }
            if matches!(
                path.last().map(|s| s.as_str()),
                Some("Int") | Some("UInt") | Some("UInt8") | Some("UInt16") | Some("UInt32")
            )
    )
}

/// The container heads that map onto C++ standard containers.
pub fn container_template(name: &str) -> Option<&'static str> {
    Some(match name {
        "Array" => "std::vector",
        "Map" => "std::map",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives() {
        assert_eq!(map_primitive("Int"), Some("int"));
        assert_eq!(map_primitive("Float"), Some("double"));
        assert_eq!(map_primitive("Float32"), Some("float"));
        assert_eq!(map_primitive("Single"), Some("float"));
        assert_eq!(map_primitive("Float64"), Some("double"));
        assert_eq!(map_primitive("String"), Some("std::string"));
        assert_eq!(map_primitive("UInt32"), Some("uint32_t"));
        // `Dynamic`/`Any` are no longer primitives — they are the overload marker.
        assert_eq!(map_primitive("Dynamic"), None);
        assert_eq!(map_primitive("Any"), None);
        assert_eq!(map_primitive("Vertex"), None);
    }

    #[test]
    fn uint_shims_and_containers() {
        assert!(is_uint_shim("UInt8"));
        assert!(!is_uint_shim("Int"));
        assert_eq!(container_template("Array"), Some("std::vector"));
        assert_eq!(container_template("Map"), Some("std::map"));
        assert_eq!(container_template("List"), None);
    }
}
