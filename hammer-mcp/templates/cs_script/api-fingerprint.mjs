// cs_script tool: compares two declarations of the module "cs_script/point_script" by what they
// declare (exports, members, signatures), not by their text. `node --input-type=module -e <this> A B`,
// run with the project as cwd (it loads the project's own `typescript`). Prints one JSON line:
// {equal, onlyA, onlyB, changed:[{name, members}]}. A = the game's file, B = the npm package's.
import { createRequire } from 'node:module';
const require = createRequire(process.cwd() + '/');
const ts = require('typescript');
const [fileA, fileB] = process.argv.slice(1);
const MOD = 'cs_script/point_script';
const FLAGS = ts.TypeFormatFlags.NoTruncation;

function describe(checker, program, file) {
  const sf = program.getSourceFile(file);
  if (!sf) throw new Error(`cannot read ${file}`);
  let modSym;
  for (const st of sf.statements) {
    if (ts.isModuleDeclaration(st) && st.name.text === MOD) modSym = checker.getSymbolAtLocation(st.name);
  }
  if (!modSym) throw new Error(`no declare module "${MOD}" in ${file}`);
  const str = (t, alias) => checker.typeToString(t, undefined, alias ? FLAGS | ts.TypeFormatFlags.InTypeAlias : FLAGS);
  const decl = (s) => s.valueDeclaration ?? s.declarations?.[0];
  const typeOf = (s) => checker.getTypeOfSymbolAtLocation(s, decl(s) ?? sf);
  const props = (t) => {
    const out = {};
    for (const p of checker.getPropertiesOfType(t)) {
      const d = decl(p);
      const mods = d ? ts.getCombinedModifierFlags(d) : 0;
      const tag = (p.flags & ts.SymbolFlags.Optional ? '?' : '') + (mods & ts.ModifierFlags.Readonly ? ' readonly' : '') + (mods & ts.ModifierFlags.Static ? ' static' : '');
      out[p.name] = `${tag}:${str(typeOf(p))}`;
    }
    for (const kind of [ts.SignatureKind.Call, ts.SignatureKind.Construct]) {
      checker.getSignaturesOfType(t, kind).forEach((sg, i) => {
        out[`<${kind === ts.SignatureKind.Call ? 'call' : 'new'}${i}>`] = checker.signatureToString(sg, undefined, FLAGS);
      });
    }
    return out;
  };
  const of = (s) => {
    const f = s.flags;
    if (f & ts.SymbolFlags.Enum) {
      const out = {};
      for (const [name, m] of s.exports ?? []) out[String(name)] = String(checker.getConstantValue(m.valueDeclaration));
      return { enum: out };
    }
    if (f & (ts.SymbolFlags.Class | ts.SymbolFlags.Interface)) {
      const t = checker.getDeclaredTypeOfSymbol(s);
      const res = { members: props(t) };
      if (f & ts.SymbolFlags.Class) res.statics = props(typeOf(s));
      return res;
    }
    if (f & ts.SymbolFlags.TypeAlias) return { alias: str(checker.getDeclaredTypeOfSymbol(s), true) };
    if (f & ts.SymbolFlags.Namespace && !(f & ts.SymbolFlags.Variable)) {
      const out = {};
      for (const e of checker.getExportsOfModule(s)) out[e.name] = of(e);
      return { namespace: out };
    }
    return { value: props(typeOf(s)), type: str(typeOf(s)) };
  };
  const out = {};
  for (const e of checker.getExportsOfModule(modSym)) out[e.name] = of(e);
  return out;
}

function load(file) {
  const program = ts.createProgram([file], { noEmit: true, types: [], noResolve: true, skipLibCheck: true, target: ts.ScriptTarget.ES2022 });
  return describe(program.getTypeChecker(), program, file);
}

const a = load(fileA);
const b = load(fileB);
const names = (o) => Object.keys(o).sort();
const diffMembers = (x, y) => {
  const flat = (v, p = '', acc = {}) => {
    if (v && typeof v === 'object') for (const k of Object.keys(v)) flat(v[k], p ? `${p}.${k}` : k, acc);
    else acc[p] = String(v);
    return acc;
  };
  const fx = flat(x), fy = flat(y);
  return [...new Set([...Object.keys(fx), ...Object.keys(fy)])].sort().filter((k) => fx[k] !== fy[k]);
};
const onlyA = names(a).filter((n) => !(n in b));
const onlyB = names(b).filter((n) => !(n in a));
const changed = names(a)
  .filter((n) => n in b)
  .map((n) => ({ name: n, members: diffMembers(a[n], b[n]) }))
  .filter((c) => c.members.length > 0);
const cap = (l) => l.slice(0, 40);
console.log(JSON.stringify({
  equal: !onlyA.length && !onlyB.length && !changed.length,
  exports: names(a).length,
  onlyA: cap(onlyA), onlyB: cap(onlyB),
  changed: changed.slice(0, 40).map((c) => ({ name: c.name, members: c.members.slice(0, 8) })),
  counts: { onlyA: onlyA.length, onlyB: onlyB.length, changed: changed.length },
}));
