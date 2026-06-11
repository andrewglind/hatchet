package examples.shapes;
import examples.shapes.Geometry;
import examples.shapes.Shape;

class Rectangle extends Shape {
	public var width(default, null):Float;
	public var height(default, null):Float;

	// `super(...)` is the first statement, so the base is built with a normal
	// C++ initializer list (`: Shape("rectangle")`) — no Holder needed. The
	// optional `height` lowers to a C++ default argument (`double height = 1.0`).
	public function new(width:Float, height:Float = 1.0) {
		super("rectangle");
		this.width = width;
		this.height = height;
	}

	override public function area():Float {
		return width * height;
	}

	override public function kind():ShapeKind {
		return RectKind;
	}
}
