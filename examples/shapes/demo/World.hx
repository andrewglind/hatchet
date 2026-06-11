package demo;

// Owns a collection of shapes and reports on them. Demonstrates owned
// containers, comprehensions, Array/Map methods, a switch over an enum, and
// string building.
class World {
	public var name(default, null):String;

	// A privately-owned container. Because the shapes below are `new`ed
	// directly into it, Hatchet's escape analysis marks it owned and the
	// generated destructor frees each element (then nothing else needs to).
	var shapes:Array<Shape>;

	public function new(name:String) {
		this.name = name;
		this.shapes = [];
		// Object literals (`{ x:…, y:… }`) become `Vec2` value temporaries.
		shapes.push(new Circle({x: 0.0, y: 0.0}, 2.0));
		shapes.push(new Rectangle(3.0, 4.0));
		shapes.push(new Circle({x: 1.0, y: 1.0}, 1.5));

		#if VERBOSE
		// Conditional compilation maps straight to the C++ preprocessor. With
		// VERBOSE undefined (the default) this whole block is `#ifdef`-ed out.
		trace("world built");
		#end
	}

	public function count():Int {
		return shapes.length;
	}

	public function totalArea():Float {
		var total:Float = 0.0;
		for (s in shapes) {
			total += s.area();
		}
		return total;
	}

	// Array comprehension -> a hoisted `std::vector<float>` filled by a loop.
	public function areas():Array<Float> {
		return [for (s in shapes) s.area()];
	}

	// Root-mean-square of the shape areas — exercises a `Math` intrinsic
	// (`Math.sqrt` -> `sqrt`) and `Std.int` (`Std.int(x)` -> `(int)(x)`).
	public function areaRms():Int {
		var sum:Float = 0.0;
		for (s in shapes) {
			sum += s.area() * s.area();
		}
		return Std.int(Math.sqrt(sum / shapes.length));
	}

	// Map methods (`exists` / `get` / `set`) over a `std::map`.
	public function tally():Map<String, Int> {
		var counts = new Map<String, Int>();
		for (s in shapes) {
			var key:String = kindName(s.kind());
			if (counts.exists(key)) {
				counts.set(key, counts.get(key) + 1);
			} else {
				counts.set(key, 1);
			}
		}
		return counts;
	}

	// A switch over an enum -> a C++ `switch` on the enum constants.
	function kindName(k:ShapeKind):String {
		switch (k) {
			case CircleKind:
				return "circle";
			case RectKind:
				return "rectangle";
		}
		return "?";
	}

	// String interpolation followed by `+` concatenation of an Int and a Float,
	// all accumulated into one overflow-safe `std::string`.
	public function report():String {
		var total:Float = totalArea();
		return 'World "$name": ' + count() + " shapes, total area " + total;
	}
}
