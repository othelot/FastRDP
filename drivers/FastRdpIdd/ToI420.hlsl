// ToI420.hlsl - BGRA -> I420 (BT.709 limited range) on the render GPU, so fastrdp-server gets 1.5
// instead of 4 bytes per pixel and the software encoder skips its colour conversion.
// The integer formula is the one of crates/encoder/src/convert.rs (bgra_to_i420), so both paths give
// identical bytes. One thread per 2x2 block. Output: one R8 texture of width x (height * 3/2):
// Y rows, then U (left half) and V (right half) of the chroma rows below them.
// Compiled at build time (FxCompile, cs_5_0) into ToI420.h (g_ToI420).

Texture2D<float4> Src : register(t0);         // B8G8R8A8_UNORM copy of the swap-chain surface
RWTexture2D<unorm float> Dst : register(u0);  // R8_UNORM, width x height*3/2

cbuffer Size : register(b0)
{
    uint Width;   // even
    uint Height;  // even
    uint2 Pad;
};

int3 Rgb(uint2 p)
{
    return int3(round(Src.Load(int3(p, 0)).rgb * 255.0));
}

int Luma(int3 c)
{
    return 16 + ((47 * c.r + 157 * c.g + 16 * c.b + 128) >> 8);
}

float Byte(int v)
{
    return (float)clamp(v, 0, 255) / 255.0;
}

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint cw = Width / 2;
    uint ch = Height / 2;
    if (id.x >= cw || id.y >= ch)
        return;
    uint2 p = id.xy * 2;
    int3 a = Rgb(p);
    int3 b = Rgb(p + uint2(1, 0));
    int3 c = Rgb(p + uint2(0, 1));
    int3 d = Rgb(p + uint2(1, 1));
    Dst[p] = Byte(Luma(a));
    Dst[p + uint2(1, 0)] = Byte(Luma(b));
    Dst[p + uint2(0, 1)] = Byte(Luma(c));
    Dst[p + uint2(1, 1)] = Byte(Luma(d));
    int3 s = (a + b + c + d + 2) >> 2;
    Dst[uint2(id.x, Height + id.y)] = Byte(128 + ((-26 * s.r - 87 * s.g + 112 * s.b + 128) >> 8));
    Dst[uint2(cw + id.x, Height + id.y)] = Byte(128 + ((112 * s.r - 102 * s.g - 10 * s.b + 128) >> 8));
}
