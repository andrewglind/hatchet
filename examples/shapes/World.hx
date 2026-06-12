package examples.shapes;
import examples.shapes.Geometry;
import examples.shapes.Shape;
import examples.shapes.Circle;
import examples.shapes.Rectangle;

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

	// Array comprehension -> a hoisted `std::vector<double>` filled by a loop.
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

	// Classify an area into a `Bucket` — produces `enum abstract` values (the bare
	// `Tiny`/`Small`/`Big` members lower to `Bucket_::Tiny`, …).
	function bucket(a:Float):Bucket {
		if (a < 8.0) return Tiny;
		if (a < 13.0) return Small;
		return Big;
	}

	// A `switch` used as an EXPRESSION on an `enum abstract` subject: each arm
	// yields a value, desugared to a hidden temporary assigned inside a `switch`.
	function bucketName(b:Bucket):String {
		return switch (b) {
			case Tiny: "tiny";
			case Small: "small";
			default: "big";
		}
	}

	// A `switch` EXPRESSION on a `String` subject -> an `if`/`else if` chain (C++
	// `case` labels must be integral, so a string subject cannot use a `switch`).
	function code(name:String):String {
		return switch (name) {
			case "circle": "C";
			case "rectangle": "R";
			default: "?";
		}
	}

	// A round-up of the newer lowerings, returning a deterministic string the test
	// checks segment by segment: `enum abstract` + `switch` expressions (enum and
	// String subjects), `StringBuf`, `StringTools`, `String.substr`/`substring`,
	// and the extra `Array` methods.
	public function features():String {
		// `enum abstract` value through a classifier, named via a `switch` expr.
		var big:String = bucketName(bucket(20.0));         // "big"
		// `switch` expression on a String subject.
		var c:String = code(kindName(shapes[0].kind()));   // "C"
		// `StringBuf` accumulator + `StringTools.replace` + `String` slicing.
		var buf = new StringBuf();
		buf.add("shapes:");
		buf.add(count());                                   // "shapes:3"
		var t:String = StringTools.replace(buf.toString(), ":", "=");  // "shapes=3"
		var head:String = t.substring(0, 6);                // "shapes"
		var tail:String = t.substr(-1);                     // "3"
		// `Array` methods: concat / slice / shift / unshift / lastIndexOf.
		var xs:Array<Int> = [1, 2, 3];
		var ys:Array<Int> = xs.concat([4, 5]);              // [1, 2, 3, 4, 5]
		var mid:Array<Int> = ys.slice(1, 4);                // [2, 3, 4]
		var first:Int = mid.shift();                        // 2 ; mid = [3, 4]
		mid.unshift(first * 10);                            // [20, 3, 4]
		var last5:Int = ys.lastIndexOf(5);                  // 4
		return big + "|" + c + "|" + head + "=" + tail + "|" + mid.join(",") + "|" + last5;
	}
}
