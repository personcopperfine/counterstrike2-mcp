// Generates tests/data/float_format_table.txt: ground truth for keyvalues2 float text.
//
// Upstream Datamodel.NET prints a float as ((double)value).ToString("0.##########", InvariantCulture).
// Each output line is `<8 hex digits of the f32 bit pattern> <the text .NET printed>`.
//
// Regenerate (needs a .NET SDK, not needed to run the Rust tests):
//   dotnet run --project crates/cs2-dmx/tests/dotnet/FloatTable -- \
//       crates/cs2-dmx/tests/data/float_format_table.txt crates/cs2-dmx/tests/data/float64_format_table.txt
// The inputs are deterministic (fixed seed), so a rerun reproduces the file byte for byte.
using System.Globalization;
using System.Numerics;
using System.Text;

var bitsSeen = new HashSet<uint>();
var bits = new List<uint>();
void Add(float f)
{
    if (float.IsNaN(f) || float.IsInfinity(f)) return;
    uint b = BitConverter.SingleToUInt32Bits(f);
    if (bitsSeen.Add(b)) bits.Add(b);
}
void AddBoth(float f) { Add(f); Add(-f); }
void AddNeighbours(float f)
{
    uint b = BitConverter.SingleToUInt32Bits(f);
    AddBoth(f);
    AddBoth(BitConverter.UInt32BitsToSingle(b + 1));
    if (b > 0) AddBoth(BitConverter.UInt32BitsToSingle(b - 1));
}

// the edges
foreach (var f in new[] { 0f, 1f, 0.1f, 0.3f, 0.5f, 2.5f, 40f, 65504f, 123456789f, float.MaxValue, float.Epsilon, 1.17549435E-38f, 1e-7f, 1e-10f, 5.96e-8f, 0.9999999999f, 1.05f, 0.015f, 0.0001f, 2000000000f })
    AddBoth(f);
Add(-0f);
Add(BitConverter.UInt32BitsToSingle(0x80000000));

// every power of two, and its neighbours
for (int e = -149; e <= 127; e++) AddNeighbours((float)Math.ScaleB(1.0, e));

// decimal ties at the tenth decimal: odd / 2^11 has exactly eleven decimals ending in 5
for (int k = 0; k < 600; k++) AddBoth((2 * k + 1) / 2048f);
var rng = new Random(20261017);
for (int i = 0; i < 300; i++)
{
    // odd * 2^-11 with up to 24 bits of odd
    uint odd = (uint)rng.Next(1, 1 << 23) * 2 + 1;
    AddBoth((float)(odd * Math.ScaleB(1.0, -11)));
}
// ties at the tenth decimal one binade down: odd / 2^12 ends in ...25 / ...75 (no tie), kept as near misses
for (int k = 0; k < 100; k++) AddBoth((2 * k + 1) / 4096f);

// the 1.8e9 .. 2^53 range, where the old formatter lost digits
for (int i = 0; i < 400; i++) AddBoth((float)(1.8e9 * Math.Pow(9.007199254740992e15 / 1.8e9, rng.NextDouble())));
for (int i = 0; i < 40; i++) AddBoth(2e9f + i * 128f);

// everything beyond 1e15 is an integer padded with zeros after fifteen digits
for (int i = 0; i < 300; i++) AddBoth((float)Math.Pow(10, 15 + rng.NextDouble() * 23.5));

// values people actually write: a few decimals, any magnitude
for (int i = 0; i < 500; i++)
{
    double scale = Math.Pow(10, rng.Next(-8, 9));
    AddBoth((float)(Math.Round(rng.NextDouble() * 1000, rng.Next(0, 8)) * scale / 1000));
}
// random bit patterns over the whole finite range
for (int i = 0; i < 700; i++) Add(BitConverter.UInt32BitsToSingle((uint)rng.NextInt64(0, 0x7f7fffff)));
for (int i = 0; i < 300; i++) Add(BitConverter.UInt32BitsToSingle((uint)rng.NextInt64(0x80000000L, 0xff7fffffL)));

var sb = new StringBuilder();
foreach (var b in bits)
{
    float f = BitConverter.UInt32BitsToSingle(b);
    sb.Append(b.ToString("x8")).Append(' ').Append(((double)f).ToString("0.##########", CultureInfo.InvariantCulture)).Append('\n');
}
File.WriteAllText(args[0], sb.ToString());
Console.WriteLine($"{bits.Count} f32 rows");

// An f32 can never be an exact tie at fifteen significant digits (a 16th digit 5 followed only by
// zeros needs 5^(k+1) | m for 2^k | value, which no 24-bit mantissa has), so the half-even rule of
// the first rounding step is checked with f64 values: <16 hex digits of the bit pattern> <text>.
var sb64 = new StringBuilder();
var seen64 = new HashSet<ulong>();
void Add64(double d)
{
    if (double.IsNaN(d) || double.IsInfinity(d)) return;
    ulong b = BitConverter.DoubleToUInt64Bits(d);
    if (!seen64.Add(b)) return;
    sb64.Append(b.ToString("x16")).Append(' ').Append(d.ToString("0.##########", CultureInfo.InvariantCulture)).Append('\n');
}
int ties64 = 0;
for (int e = 0; e <= 60 && ties64 < 150; e++)
{
    for (int attempt = 0; attempt < 20000 && ties64 < 150; attempt++)
    {
        // m * 2^e whose decimal expansion has a 16th digit 5 and nothing after it
        ulong m = (ulong)rng.NextInt64(1L << 40, 1L << 53);
        BigInteger v = new BigInteger(m) << e;
        string s = v.ToString();
        if (s.Length >= 16 && s[15] == '5' && s.Substring(16).All(c => c == '0'))
        {
            Add64((double)v); Add64(-(double)v); ties64++;
        }
    }
}
// plus fractional ties: odd / 2^k with a 16th significant digit 5 and nothing after it
for (int k = 1; k <= 60; k++)
{
    for (int attempt = 0; attempt < 2000; attempt++)
    {
        ulong odd = (ulong)rng.NextInt64(1L << 30, 1L << 53) | 1;
        double d = odd / Math.ScaleB(1.0, k);
        BigInteger scaled = new BigInteger(odd) * BigInteger.Pow(5, k);
        string t = scaled.ToString();
        if (t.Length == 16 && t[15] == '5') { Add64(d); }
        if (t.Length >= 17 && t.Length <= 40 && t[15] == '5' && t.Substring(16).All(c => c == '0')) { Add64(d); Add64(-d); }
    }
}
for (int i = 0; i < 300; i++) Add64(rng.NextDouble() * Math.Pow(10, rng.Next(-5, 25)));
for (int i = 0; i < 200; i++) Add64(BitConverter.UInt64BitsToDouble((ulong)rng.NextInt64(0, 0x7fefffffffffffffL)));
File.WriteAllText(args[1], sb64.ToString());
Console.WriteLine($"{seen64.Count} f64 rows ({ties64} integer ties)");
