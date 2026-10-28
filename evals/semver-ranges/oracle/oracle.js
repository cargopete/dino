const semver = require("semver");
const groups = {
  primitive: ["1.2.3", "=1.2.3", "v1.2.3", ">1.2.3", ">=1.2.3", "<1.2.3", "<=1.2.3", ">= 1.2.3", "< 2", ">1", ">1.2", "<1.2", "<=1.2", ">=1.2", "<=1", "=1.2", ">=1.2.3 <2.0.0", ">1.0.0 <=1.5.0 >=1.2.0", "<0.0.1", ">=0.0.0"],
  wildcard: ["*", "", "x", "1.x", "1.X", "1.*", "1.2.x", "1", "1.2", "0.x", "1.x.x", "1.2.*", ">*", "<*", ">=*", "<=1.x", ">1.x", "<1.x"],
  tilde: ["~1.2.3", "~1.2", "~1", "~0.2.3", "~0.0.1", "~1.2.3-beta.2", "~ 1.2.3", "~1.x", "~0", "~*", "~>1.2", "~> 1.2.3"],
  caret: ["^1.2.3", "^0.2.3", "^0.0.3", "^1.2.x", "^0.0.x", "^0.0", "^1.x", "^0.x", "^1.2.3-beta.2", "^0.0.3-beta", "^0.2.3-rc.1", "^ 1.2", "^0", "^1", "^*", "^0.0.0"],
  hyphen: ["1.2.3 - 2.3.4", "1.2 - 2.3.4", "1.2.3 - 2.3", "1.2.3 - 2", "1 - 2", "1.2.3-beta - 2.0.0", "1.2.3 - 2.0.0-rc.1", "0.1 - 0.2", "1.2.3 - 2.3.4-beta"],
  union: ["1.2.7 || >=1.2.9 <2.0.0", "<1.0.0 || >=2.0.0", "^1.2.3 || ^2.0.0 || ^3.0.0-beta", "1.x || ", " || 2.x", "~1.2.3 || 1.2.0 - 1.2.2", "1.2.3||2.3.4", "<1.2.3-beta.2 || 3"],
  prerelease: [">1.2.3-alpha.3", ">=1.2.3-beta <1.2.4", "<1.2.3-beta.2", "1.2.3-alpha.1", ">=1.0.0-rc.1 <1.0.0", ">=2.0.0-0", "<2.0.0-0", ">=1.2.3-alpha.1 <=1.2.3-beta.2", "1.2.3-beta.2 - 1.2.3"],
  invalid: ["abc", ">=a.b.c", "1.2.3.4", "1.2.03", ">>1.2.3", "^1.2.3.4", "1.2.3-", "1.2-beta", ">=", "1.2.3 -2.0.0", "1.2.3 - ", "=>1.2.3", "~>", "1.2.3 1.2.4 - 2"],
};
const versions = "0.0.0 0.0.1 0.0.2 0.0.3 0.0.4 0.1.0 0.2.0 0.2.3 0.2.9 0.3.0 0.9.9 1.0.0-0 1.0.0-rc.1 1.0.0-rc.2 1.0.0 1.0.1 1.1.9 1.2.0-0 1.2.0 1.2.2 1.2.3-alpha.1 1.2.3-alpha.3 1.2.3-alpha.10 1.2.3-beta 1.2.3-beta.2 1.2.3-beta.11 1.2.3 1.2.3+build.7 1.2.4-0 1.2.4 1.2.7 1.2.8 1.2.9 1.3.0-0 1.3.0 1.5.0 1.9.9 2.0.0-0 2.0.0-rc.1 2.0.0 2.3.3 2.3.4 2.3.4-beta 2.3.5 2.4.0-0 2.4.0 2.9.9 3.0.0-beta.1 3.0.0 10.0.0".split(" ");
const out = { versions, ranges: [] };
for (const [group, list] of Object.entries(groups)) {
  for (const r of list) {
    const valid = semver.validRange(r) !== null;
    out.ranges.push({ group, range: r, valid, matches: valid ? versions.filter((v) => semver.satisfies(v, r)) : [] });
  }
}
const vstrings = ["1.2.3", "v1.2.3", " 1.2.3 ", "0.0.0", "1.2.3-alpha", "1.2.3-alpha.1", "1.2.3-0.3.7", "1.2.3-x.7.z.92", "1.2.3+20130313144700", "1.2.3-beta+exp.sha.5114f85", "1.2.3+001", "1.2.3-x-y-z.--", "1.2", "1.2.3.4", "01.2.3", "1.02.3", "1.2.03", "1.2.3-01", "1.2.3-", "1.2.3+", "1.2.3-a..b", "1.2.3-a_b", "a.b.c", "", "1.2.3-alpha+", "=1.2.3", "V1.2.3", "1.2.3 4"];
out.parse = vstrings.map((s) => ({ input: s, valid: semver.valid(s) !== null, display: semver.valid(s) !== null ? (() => { const p = semver.parse(s); return p.version + (p.build.length ? "+" + p.build.join(".") : ""); })() : null }));
const order = "1.0.0-alpha 1.0.0-alpha.1 1.0.0-alpha.beta 1.0.0-beta 1.0.0-beta.2 1.0.0-beta.11 1.0.0-rc.1 1.0.0 1.0.0+build 1.0.1 1.1.0 2.0.0 10.0.0 1.0.0-2 1.0.0-10 1.0.0-a-b 1.0.0-A 1.0.0-alpha.1.1".split(" ");
out.compare = [];
for (const a of order) for (const b of order) out.compare.push([a, b, semver.compare(a, b)]);
out.max = [["^1.2.3", ["1.2.3", "1.9.9", "2.0.0", "1.10.0", "1.2.4-beta"]], ["~1.2", ["1.2.0", "1.2.10", "1.2.9", "1.3.0"]], [">=2.0.0", ["1.0.0", "1.5.0"]], ["*", ["1.0.0+a", "1.0.0+b", "0.9.0"]], ["1.2.3-beta - 1.2.3", ["1.2.3-alpha", "1.2.3-beta.3", "1.2.3-rc.1", "1.2.3"]]]
  .map(([r, vs]) => ({ range: r, versions: vs, max: semver.maxSatisfying(vs, r), min: semver.minSatisfying(vs, r) }));
require("fs").writeFileSync("oracle.json", JSON.stringify(out, null, 1));
console.log(out.ranges.length, "ranges;", out.ranges.filter((r) => !r.valid).map((r) => JSON.stringify(r.range)).join(" "));
console.log(out.max.map(m=>JSON.stringify(m)).join("\n"));
