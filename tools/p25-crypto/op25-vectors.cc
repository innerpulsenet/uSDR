#include <cstdio>
#include <cstdlib>
#include "op25_crypt_algs.h"
int main() {
 log_ts log; op25_crypt_algs alg(log,0,0);
 for (int id : {0xaa,0x81,0x84}) {
  std::vector<uint8_t> key(id==0xaa?5:id==0x81?8:32);
  for (size_t i=0;i<key.size();i++) key[i]=i;
  alg.key(0x1234,id,key);
  uint8_t mi[9]={0x12,0x34,0x56,0x78,0x9a,0xbc,0xde,0xf0,0};
  for (auto protocol : {PT_P25_PHASE1,PT_P25_PHASE2}) {
   alg.prepare(id,0x1234,protocol,mi);
   printf("%02x %d ",id,protocol);
   for(int n=0;n<18;n++) {
    packed_codeword cw(protocol==PT_P25_PHASE1?11:7,0);
    frame_type type=protocol==PT_P25_PHASE1?(n<9?FT_LDU1:FT_LDU2):(n<16?static_cast<frame_type>(FT_4V_0+n/4):FT_2V);
    alg.process(cw,type,n<16?n%4:n-16);
    for(auto v:cw) printf("%02x",v);
   }
   puts("");
  }
 }
 fflush(stdout); std::_Exit(0);
}
