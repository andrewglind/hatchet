package examples.shapes;

// A 2-D point. A `typedef` of an anonymous structure lowers to a C++98 `struct`
// (`struct Vec2 { double x; double y; };`) and is passed by value — there is no
// heap allocation and nothing to own. Construct one with an object literal:
// `{ x: 1.0, y: 2.0 }`, which Hatchet expands into a named temporary.
typedef Vec2 = { x:Float, y:Float };

// A plain enum lowers to the pre-C++11 idiom: `struct ShapeKind_ { enum Enum {
// CircleKind, RectKind }; }; typedef ShapeKind_::Enum ShapeKind;`.
enum ShapeKind {
	CircleKind;
	RectKind;
}

// An `Int`-backed `enum abstract` lowers to the *same* C++ enum idiom, but its
// members carry explicit values. `Tiny`/`Small` auto-increment from 0; `Big` is
// given an explicit value (so this also shows the `Name = <value>` form). It is a
// typed set of Int constants — `World` uses it to bucket shapes by area.
enum abstract Bucket(Int) {
	var Tiny:Bucket; 	// 0
	var Small:Bucket; 	// 1
	var Big:Bucket = 9;	// explicit value
}
