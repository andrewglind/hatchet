// Hand-written entry point for the transpiled `shapes` example.
//
// Hatchet never transpiles `Main.hx` (it is the hxcpp entry point), so a real
// build supplies its own `main()`. This one constructs an `examples::shapes::World`,
// prints its report, walks the tally, and prints the feature probe — exercising
// the generated C++ end to end.
//
// It is deliberately written in plain, VC6-friendly C++98: C standard headers,
// `printf`, and a loop variable declared outside the `for` (VC6 leaks the
// `for`-init into the enclosing scope).
//
// Build (after transpiling to ./out — see README.md):
//   g++ -std=c++98 -I out main.cpp out/examples/shapes/*.cpp -o shapes

#include <stdio.h>
#include <string>
#include <map>

#include "examples/shapes/World.h"

int main() {
	examples::shapes::World world("shapes");

	printf("%s\n", world.report().c_str());
	printf("shape count: %d\n", world.count());
	printf("area rms: %d\n", world.areaRms());
	printf("features: %s\n", world.features().c_str());

	std::map<std::string, int> counts = world.tally();
	std::map<std::string, int>::iterator it;
	for (it = counts.begin(); it != counts.end(); ++it) {
		printf("  %s x%d\n", it->first.c_str(), it->second);
	}

	return 0;
}
