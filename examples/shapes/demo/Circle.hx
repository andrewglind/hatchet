package demo;

class Circle extends Shape {
	public var center(default, null):Vec2;
	public var radius(default, null):Float;

	public function new(center:Vec2, radius:Float) {
		// Doing work BEFORE `super(...)` forces Hatchet's base-from-member
		// "Holder" idiom: a private `CircleHolder` base runs this pre-super
		// logic and stores the result, which the `Shape` base is then
		// constructed from. (Also exercises String + Float concatenation.)
		var label:String = "circle r=" + radius;
		super(label);
		this.center = center;
		this.radius = radius;
	}

	override public function area():Float {
		return 3.14159 * radius * radius;
	}

	override public function kind():ShapeKind {
		return CircleKind;
	}
}
