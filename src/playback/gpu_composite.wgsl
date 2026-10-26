struct Params { v: array<vec4<f32>,10> }
@group(0) @binding(0) var<storage,read> picture: array<f32>;
@group(0) @binding(1) var<storage,read_write> layer: array<vec4<f32>>;
@group(0) @binding(2) var<storage,read_write> output: array<f32>;
@group(0) @binding(3) var<uniform> p: Params;
fn pixel(x:u32,y:u32)->vec3<f32>{let at=(y*u32(p.v[0].z)+x)*3u;return vec3(picture[at],picture[at+1u],picture[at+2u]);}
fn sample(uv:vec2<f32>)->vec3<f32>{let size=p.v[0].zw;let xy=clamp(uv*size-vec2(0.5),vec2(0.),size-vec2(1.));let lo=vec2<u32>(floor(xy));let hi=min(lo+vec2(1u),vec2<u32>(size)-vec2(1u));let t=fract(xy);return mix(mix(pixel(lo.x,lo.y),pixel(hi.x,lo.y),t.x),mix(pixel(lo.x,hi.y),pixel(hi.x,hi.y),t.x),t.y);}
fn grade(input:vec3<f32>)->vec3<f32>{
    var c=(input*p.v[5].x-vec3(0.5))*p.v[5].y+vec3(0.5);
    let high=smoothstep(vec3(0.5),vec3(1.),c);if p.v[5].z>=0.{c+=p.v[5].z*high*(vec3(1.)-c);}else{c+=p.v[5].z*high*c;}
    let shadow=vec3(1.)-smoothstep(vec3(0.),vec3(0.5),c);if p.v[5].w>=0.{c+=p.v[5].w*shadow*0.5*(vec3(1.)-c);}else{c+=p.v[5].w*shadow*0.5*c;}
    c+=vec3(p.v[6].x,-p.v[6].y,-p.v[6].x);let luma=dot(c,vec3(0.2126,0.7152,0.0722));c=vec3(luma)+(c-vec3(luma))*p.v[6].z;
    c=(c+p.v[7].xyz*(vec3(1.)-c))*p.v[9].xyz;return pow(clamp(c,vec3(0.),vec3(1.)),p.v[8].xyz);
}
@compute @workgroup_size(8,8)
fn main(@builtin(global_invocation_id) gid:vec3<u32>){
    let w=u32(p.v[0].x);let h=u32(p.v[0].y);if gid.x>=w||gid.y>=h{return;}let at=gid.y*w+gid.x;let base=at*3u;let phase=u32(p.v[4].w);
    if phase==0u{layer[at]=vec4(0.);return;}
    if phase==2u{let src=layer[at];let inverse=1.-clamp(src.w,0.,1.);output[base]=src.x+output[base]*inverse;output[base+1u]=src.y+output[base+1u]*inverse;output[base+2u]=src.z+output[base+2u]*inverse;return;}
    if phase==3u{layer[at]+=vec4(0.,0.,0.,p.v[4].z);return;}
    let delta=vec2<f32>(gid.xy)+vec2(0.5)-p.v[1].xy;
    var uv=vec2((delta.x*p.v[2].y+delta.y*p.v[2].x)/p.v[1].z,(-delta.x*p.v[2].x+delta.y*p.v[2].y)/p.v[1].w)+p.v[2].zw;
    if p.v[4].x>0.5{uv.x=1.-uv.x;}if p.v[4].y>0.5{uv.y=1.-uv.y;}
    if uv.x<p.v[3].x||uv.x>=1.-p.v[3].y||uv.y<p.v[3].z||uv.y>=1.-p.v[3].w{return;}
    layer[at]+=vec4(grade(sample(uv))*p.v[4].z,p.v[4].z);
}
