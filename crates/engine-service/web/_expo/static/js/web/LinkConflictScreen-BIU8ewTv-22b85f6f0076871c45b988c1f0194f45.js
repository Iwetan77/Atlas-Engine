__d(function(_g,r,_i,_a,_m,_e,_d){"use strict";function e(e){return e&&e.__esModule?e:{default:e}}Object.defineProperty(_e,'__esModule',{value:!0}),Object.defineProperty(_e,"LinkConflictScreen",{enumerable:!0,get:function(){return $}}),Object.defineProperty(_e,"LinkConflictScreenView",{enumerable:!0,get:function(){return z}}),Object.defineProperty(_e,"default",{enumerable:!0,get:function(){return $}});var n=r(_d[0]),t=e(r(_d[1])),s=e(r(_d[2])),o=r(_d[3]),a=r(_d[4]),i=r(_d[5]),c=r(_d[6]),l=r(_d[7]),d=r(_d[8]),u=r(_d[9]),f=r(_d[10]),h=r(_d[11]),x=r(_d[12]),p=e(r(_d[13])),g=e(r(_d[14]));r(_d[15]),r(_d[16]),r(_d[17]),r(_d[18]),r(_d[19]),r(_d[20]),r(_d[21]),r(_d[22]),r(_d[23]),r(_d[24]),r(_d[25]),r(_d[26]),r(_d[27]),r(_d[28]),r(_d[29]);const y=i.styled.span`
  && {
    width: 82px;
    height: 82px;
    border-width: 4px;
    border-style: solid;
    border-color: ${e=>e.color??"var(--privy-color-accent)"};
    border-radius: 50%;
    display: inline-block;
    box-sizing: border-box;
    animation: rotation 1.2s linear infinite;
    transition: border-color 800ms;
  }
`;function m(e){return(0,n.jsxs)("svg",{xmlns:"http://www.w3.org/2000/svg",width:"24",height:"24",viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":"2","stroke-linecap":"round","stroke-linejoin":"round",...e,children:[(0,n.jsx)("circle",{cx:"12",cy:"12",r:"10"}),(0,n.jsx)("line",{x1:"12",x2:"12",y1:"8",y2:"12"}),(0,n.jsx)("line",{x1:"12",x2:"12.01",y1:"16",y2:"16"})]})}const j=({onTransfer:e,isTransferring:t,transferSuccess:s})=>(0,n.jsx)(a.P,{...s?{success:!0,children:"Success!"}:{warn:!0,loading:t,onClick:e,children:"Transfer and delete account"}}),b=i.styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 8px;
  width: 100%;
  padding-bottom: 16px;
`,v=i.styled.div`
  display: flex;
  flex-direction: column;
  && p {
    font-size: 14px;
  }
  width: 100%;
  gap: 16px;
`,T=i.styled.div`
  display: flex;
  cursor: pointer;
  align-items: center;
  width: 100%;
  border: 1px solid var(--privy-color-foreground-4) !important;
  border-radius: var(--privy-border-radius-md);
  padding: 8px 10px;
  font-size: 14px;
  font-weight: 500;
  gap: 8px;
`,w=(0,i.styled)(p.default)`
  position: relative;
  width: ${({$iconSize:e})=>`${e}px`};
  height: ${({$iconSize:e})=>`${e}px`};
  color: var(--privy-color-foreground-3);
  margin-left: auto;
`,k=(0,i.styled)(g.default)`
  position: relative;
  width: 15px;
  height: 15px;
  color: var(--privy-color-foreground-3);
  margin-left: auto;
`,C=i.styled.ol`
  display: flex;
  flex-direction: column;
  font-size: 14px;
  width: 100%;
  text-align: left;
`,A=i.styled.li`
  font-size: 14px;
  list-style-type: auto;
  list-style-position: outside;
  margin-left: 1rem;
  margin-bottom: 0.5rem; /* Adjust the margin as needed */

  &:last-child {
    margin-bottom: 0; /* Remove margin from the last item */
  }
`,S=i.styled.div`
  position: relative;
  width: 60px;
  height: 60px;
  margin: 10px;
  display: flex;
  justify-content: center;
  align-items: center;
`;let W=()=>(0,n.jsx)(S,{children:(0,n.jsx)(w,{$iconSize:60})});const M=({address:e,onClose:t,onRetry:o,onTransfer:i,isTransferring:l,transferSuccess:u})=>{let{defaultChain:f}=(0,x.a)(),h=f.blockExplorers?.default.url??"https://etherscan.io";return(0,n.jsxs)(n.Fragment,{children:[(0,n.jsx)(a.M,{onClose:t,backFn:o}),(0,n.jsxs)(b,{children:[(0,n.jsx)(W,{}),(0,n.jsxs)(v,{children:[(0,n.jsx)("h3",{children:"Check account assets before transferring"}),(0,n.jsx)("p",{children:"Before transferring, ensure there are no assets in the other account. Assets in that account will not transfer automatically and may be lost."}),(0,n.jsxs)(C,{children:[(0,n.jsx)("p",{children:" To check your balance, you can:"}),(0,n.jsx)(A,{children:"Log out and log back into the other account, or "}),(0,n.jsxs)(A,{children:["Copy your wallet address and use a"," ",(0,n.jsx)("u",{children:(0,n.jsx)("a",{target:"_blank",href:h,children:"block explorer"})})," ","to see if the account holds any assets."]})]}),(0,n.jsxs)(T,{onClick:()=>navigator.clipboard.writeText(e).catch(console.error),children:[(0,n.jsx)(s.default,{color:"var(--privy-color-foreground)",strokeWidth:2,height:"28px",width:"28px"}),(0,n.jsx)(d.A,{address:e,showCopyIcon:!1}),(0,n.jsx)(k,{})]}),(0,n.jsx)(j,{onTransfer:i,isTransferring:l,transferSuccess:u})]})]}),(0,n.jsx)(c.B,{})]})},$={component:()=>{let{initiateAccountTransfer:e,closePrivyModal:t}=(0,u.u)(),{data:s,navigate:a,lastScreen:i,setModalData:c}=(0,f.u)(),[l,d]=(0,o.useState)(void 0),[h,x]=(0,o.useState)(!1),[p,g]=(0,o.useState)(!1),y=async()=>{try{if(!s?.accountTransfer?.nonce||!s?.accountTransfer?.account)throw Error("missing account transfer inputs");g(!0),await e({nonce:s?.accountTransfer?.nonce,account:s?.accountTransfer?.account,accountType:s?.accountTransfer?.linkMethod,externalWalletMetadata:s?.accountTransfer?.externalWalletMetadata,telegramWebAppData:s?.accountTransfer?.telegramWebAppData,telegramAuthResult:s?.accountTransfer?.telegramAuthResult,farcasterEmbeddedAddress:s?.accountTransfer?.farcasterEmbeddedAddress,oAuthUserInfo:s?.accountTransfer?.oAuthUserInfo}),x(!0),g(!1),setTimeout(t,1e3)}catch(e){c({errorModalData:{error:e,previousScreen:i||"LinkConflictScreen"}}),a("ErrorScreen",!0)}};return l?(0,n.jsx)(M,{address:l,onClose:t,onRetry:()=>d(void 0),onTransfer:y,isTransferring:p,transferSuccess:h}):(0,n.jsx)(z,{onClose:t,onInfo:()=>d(s?.accountTransfer?.embeddedWalletAddress),onContinue:()=>d(s?.accountTransfer?.embeddedWalletAddress),onTransfer:y,isTransferring:p,transferSuccess:h,data:s})}},z=({onClose:e,onContinue:s,onInfo:o,onTransfer:i,transferSuccess:d,isTransferring:u,data:f})=>{if(!f?.accountTransfer?.linkMethod||!f?.accountTransfer?.displayName)return;let x={method:f?.accountTransfer?.linkMethod,handle:f?.accountTransfer?.displayName,disclosedAccount:f?.accountTransfer?.embeddedWalletAddress?{type:"wallet",handle:f?.accountTransfer?.embeddedWalletAddress}:void 0};return(0,n.jsxs)(n.Fragment,{children:[(0,n.jsx)(a.M,{closeable:!0}),(0,n.jsxs)(b,{children:[(0,n.jsx)(l.e,{children:(0,n.jsxs)("div",{children:[(0,n.jsx)(y,{color:"var(--privy-color-error)"}),(0,n.jsx)(t.default,{height:38,width:38,stroke:"var(--privy-color-error)"})]})}),(0,n.jsxs)(v,{children:[(0,n.jsxs)("h3",{children:[(function(e){switch(e){case"sms":return"Phone number";case"email":return"Email address";case"siwe":return"Wallet address";case"siws":return"Solana wallet address";case"linkedin":return"LinkedIn profile";case"google":case"apple":case"discord":case"github":case"instagram":case"spotify":case"tiktok":case"line":case"twitch":case"twitter":case"telegram":case"farcaster":return`${(0,h.e)(e.replace("_oauth",""))} profile`;default:return e.startsWith("privy:")?"Cross-app account":e}})(x.method)," is associated with another account"]}),(0,n.jsxs)("p",{children:["Do you want to transfer",(0,n.jsx)("b",{children:x.handle?` ${x.handle}`:""})," to this account instead? This will delete your other account."]}),(0,n.jsx)(P,{onClick:o,disclosedAccount:x.disclosedAccount})]}),(0,n.jsxs)(v,{style:{gap:12,marginTop:12},children:[f?.accountTransfer?.embeddedWalletAddress?(0,n.jsx)(a.P,{onClick:s,children:"Continue"}):(0,n.jsx)(j,{onTransfer:i,transferSuccess:d,isTransferring:u}),(0,n.jsx)(a.S,{onClick:e,children:"No thanks"})]})]}),(0,n.jsx)(c.B,{})]})};function P({disclosedAccount:e,onClick:t}){return e?(0,n.jsxs)(T,{onClick:t,children:[(0,n.jsx)(s.default,{color:"var(--privy-color-foreground)",strokeWidth:2,height:"28px",width:"28px"}),(0,n.jsx)(d.A,{address:e.handle,showCopyIcon:!1}),(0,n.jsx)(m,{width:15,height:15,color:"var(--privy-color-foreground-3)",style:{marginLeft:"auto"}})]}):null}},4404,[11,4538,4505,18,4461,3663,4467,4776,4516,2415,3677,4777,2412,4774,4775,3895,4462,4463,4464,4465,4459,2425,2420,2421,2426,1000,1411,2413,2217,2414]);
__d(function(g,r,i,a,m,e,d){const t=r(d[0]);function n({title:n,titleId:o,...l},s){return t.createElement("svg",Object.assign({xmlns:"http://www.w3.org/2000/svg",fill:"none",viewBox:"0 0 24 24",strokeWidth:1.5,stroke:"currentColor","aria-hidden":"true","data-slot":"icon",ref:s,"aria-labelledby":o},l),n?t.createElement("title",{id:o},n):null,t.createElement("path",{strokeLinecap:"round",strokeLinejoin:"round",d:"M16.5 8.25V6a2.25 2.25 0 0 0-2.25-2.25H6A2.25 2.25 0 0 0 3.75 6v8.25A2.25 2.25 0 0 0 6 16.5h2.25m8.25-8.25H18a2.25 2.25 0 0 1 2.25 2.25V18A2.25 2.25 0 0 1 18 20.25h-7.5A2.25 2.25 0 0 1 8.25 18v-1.5m8.25-8.25h-6a2.25 2.25 0 0 0-2.25 2.25v6"}))}const o=t.forwardRef(n);m.exports=o},4775,[18]);