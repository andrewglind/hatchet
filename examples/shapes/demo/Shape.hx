package demo;

// The abstract base for every shape. `World` owns its shapes and deletes them
// through this base pointer, so Hatchet gives `Shape` a `virtual ~Shape()`
// automatically — deleting a `Circle` through a `Shape*` is well defined.
class Shape {
	// `(default, null)` -> a private backing field plus a read-only `GetName()`
	// accessor (no setter).
	public var name(default, null):String;

	public function new(name:String) {
		this.name = name;
	}

	// Overridden by subclasses; the base just returns 0.
	public function area():Float {
		return 0.0;
	}

	public function kind():ShapeKind {
		return CircleKind;
	}

	// String interpolation, built as an overflow-safe `std::string`.
	public function describe():String {
		var a:Float = area();
		return '$name: area=$a';
	}
}
