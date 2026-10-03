__d(function(_g,_r,_i,_a,_m,_e,_d){"use strict";Object.defineProperty(_e,'__esModule',{value:!0}),Object.defineProperty(_e,"FarcasterConnectStatusScreen",{enumerable:!0,get:function(){return T}}),Object.defineProperty(_e,"FarcasterConnectStatusView",{enumerable:!0,get:function(){return j}}),Object.defineProperty(_e,"default",{enumerable:!0,get:function(){return T}});var e=_r(_d[0]),t=_r(_d[1]),r=_r(_d[2]),a=_r(_d[3]),i=_r(_d[4]),n=_r(_d[5]),o=_r(_d[6]),s=_r(_d[7]),l=_r(_d[8]),c=_r(_d[9]),d=_r(_d[10]),u=_r(_d[11]),h=_r(_d[12]),p=_r(_d[13]),m=_r(_d[14]),g=_r(_d[15]),f=_r(_d[16]);_r(_d[17]),_r(_d[18]),_r(_d[19]),_r(_d[20]),_r(_d[21]),_r(_d[22]),_r(_d[23]),_r(_d[24]),_r(_d[25]),_r(_d[26]),_r(_d[27]),_r(_d[28]),_r(_d[29]),_r(_d[30]),_r(_d[31]),_r(_d[32]),_r(_d[33]),_r(_d[34]),_r(_d[35]),_r(_d[36]),_r(_d[37]),_r(_d[38]),_r(_d[39]),_r(_d[40]),_r(_d[41]),_r(_d[42]),_r(_d[43]),_r(_d[44]),_r(_d[45]),_r(_d[46]),_r(_d[47]),_r(_d[48]),_r(_d[49]),_r(_d[50]),_r(_d[51]),_r(_d[52]),_r(_d[53]),_r(_d[54]),_r(_d[55]),_r(_d[56]),_r(_d[57]),_r(_d[58]),_r(_d[59]),_r(_d[60]),_r(_d[61]),_r(_d[62]),_r(_d[63]),_r(_d[64]),_r(_d[65]),_r(_d[66]),_r(_d[67]),_r(_d[68]);let y=a.styled.div`
  width: 100%;
`,v=a.styled.div`
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.75rem;
  padding: 0.75rem;
  height: 56px;
  background: ${e=>e.$disabled?"var(--privy-color-background-2)":"var(--privy-color-background)"};
  border: 1px solid var(--privy-color-foreground-4);
  border-radius: var(--privy-border-radius-md);

  &:hover {
    border-color: ${e=>e.$disabled?"var(--privy-color-foreground-4)":"var(--privy-color-foreground-3)"};
  }
`,x=a.styled.div`
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
`,b=a.styled.span`
  display: block;
  font-size: 16px;
  line-height: 24px;
  color: ${e=>e.$disabled?"var(--privy-color-foreground-2)":"var(--privy-color-foreground)"};
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  /* Single-line truncation: as a flex item this would otherwise be floored at its
     min-content width, so min-width: 0 lets it shrink and the ellipsis land at the
     container edge. */
  min-width: 0;

  @media (min-width: 441px) {
    font-size: 14px;
    line-height: 20px;
  }
`,S=(0,a.styled)(b)`
  color: var(--privy-color-foreground-3);
  font-style: italic;
`,w=(0,a.styled)(c.L)`
  margin-bottom: 0.5rem;
`,E=(0,a.styled)(l.S)`
  && {
    gap: 0.375rem;
    font-size: 14px;
    flex-shrink: 0;
  }
`;const C=({value:r,title:a,placeholder:i,className:n,showCopyButton:o=!0,truncate:l,maxLength:c=40,disabled:d=!1})=>{let[u,h]=(0,t.useState)(!1),p=l&&r?((e,t,r)=>{if((e=e.startsWith("https://")?e.slice(8):e).length<=r)return e;if("middle"===t){let t=Math.ceil(r/2)-2,a=Math.floor(r/2)-1;return`${e.slice(0,t)}...${e.slice(-a)}`}return`${e.slice(0,r-3)}...`})(r,l,c):r;return(0,t.useEffect)(()=>{if(u){let e=setTimeout(()=>h(!1),3e3);return()=>clearTimeout(e)}},[u]),(0,e.jsxs)(y,{className:n,children:[a&&(0,e.jsx)(w,{children:a}),(0,e.jsxs)(v,{$disabled:d,children:[(0,e.jsx)(x,{children:r?(0,e.jsx)(b,{$disabled:d,title:r,children:p}):(0,e.jsx)(S,{$disabled:d,children:i||"No value"})}),o&&r&&(0,e.jsx)(E,{onClick:function(e){e.stopPropagation(),navigator.clipboard.writeText(r).then(()=>h(!0)).catch(console.error)},size:"sm",children:(0,e.jsxs)(e.Fragment,u?{children:["Copied",(0,e.jsx)(s.Check,{size:14})]}:{children:["Copy",(0,e.jsx)(s.Copy,{size:14})]})})]})]})},j=({connectUri:t,loading:a,success:s,errorMessage:l,onBack:c,onClose:d,onOpenFarcaster:u})=>(0,e.jsx)(g.S,r.isMobile||a?r.isIOS?{title:l?l.message:"Sign in with Farcaster",subtitle:l?l.detail:"To sign in with Farcaster, please open the Farcaster app.",icon:f.F,iconVariant:"loading",iconLoadingStatus:{success:s,fail:!!l},primaryCta:t&&u?{label:"Open Farcaster app",onClick:u}:void 0,onBack:c,onClose:d,watermark:!0}:{title:l?l.message:"Signing in with Farcaster",subtitle:l?l.detail:"This should only take a moment",icon:f.F,iconVariant:"loading",iconLoadingStatus:{success:s,fail:!!l},onBack:c,onClose:d,watermark:!0,children:t&&r.isMobile&&(0,e.jsx)(k,{children:(0,e.jsx)(n.O,{text:"Take me to Farcaster",url:t,color:"#8a63d2"})})}:{title:"Sign in with Farcaster",subtitle:"Scan with your phone's camera to continue.",onBack:c,onClose:d,watermark:!0,children:(0,e.jsxs)(A,{children:[(0,e.jsx)(F,{children:t?(0,e.jsx)(o.Q,{url:t,size:275,squareLogoElement:f.F}):(0,e.jsx)(L,{children:(0,e.jsx)(i.L,{})})}),(0,e.jsxs)(O,{children:[(0,e.jsx)(_,{children:"Or copy this link and paste it into a phone browser to open the Farcaster app."}),t&&(0,e.jsx)(C,{value:t,truncate:"end",maxLength:30,showCopyButton:!0,disabled:!0})]})]})}),T={component:()=>{let{authenticated:r,logout:a,ready:i,user:n}=(0,d.u)(),{lastScreen:o,navigate:s,navigateBack:l,setModalData:c}=(0,p.u)(),g=(0,d.a)(),{getAuthFlow:f,loginWithFarcaster:y,closePrivyModal:v,createAnalyticsEvent:x}=(0,h.u)(),[b,S]=(0,t.useState)(void 0),[w,E]=(0,t.useState)(!1),[C,T]=(0,t.useState)(!1),k=(0,t.useRef)([]),A=f(),F=A?.meta.connectUri;return(0,t.useEffect)(()=>{let e=Date.now(),t=setInterval(async()=>{let r=await A.pollForReady.execute(),a=Date.now()-e;if(r){clearInterval(t),E(!0);try{await y(),T(!0)}catch(e){let t={retryable:!1,message:"Authentication failed"};if(e?.privyErrorCode===u.a.ALLOWLIST_REJECTED)return void s("AllowlistRejectionScreen");if(e?.privyErrorCode===u.a.USER_LIMIT_REACHED)return console.error(new u.j(e).toString()),void s("UserLimitReachedScreen");if(e?.privyErrorCode===u.a.USER_DOES_NOT_EXIST)return void s("AccountNotFoundScreen");if(e?.privyErrorCode===u.a.LINKED_TO_ANOTHER_USER)t.detail=e.message??"This account has already been linked to another user.";else{if(e?.privyErrorCode===u.a.ACCOUNT_TRANSFER_REQUIRED&&e.data?.data?.nonce)return c({accountTransfer:{nonce:e.data?.data?.nonce,account:e.data?.data?.subject,displayName:e.data?.data?.account?.displayName,linkMethod:"farcaster",embeddedWalletAddress:e.data?.data?.otherUser?.embeddedWalletAddress,farcasterEmbeddedAddress:e.data?.data?.otherUser?.farcasterEmbeddedAddress}}),void s("LinkConflictScreen");e?.privyErrorCode===u.a.INVALID_CREDENTIALS?(t.retryable=!0,t.detail="Something went wrong. Try again."):e?.privyErrorCode===u.a.TOO_MANY_REQUESTS&&(t.detail="Too many requests. Please wait before trying again.")}S(t)}}else a>12e4&&(clearInterval(t),S({retryable:!0,message:"Authentication failed",detail:"The request timed out. Try again."}))},2e3);return()=>{clearInterval(t),k.current.forEach(e=>clearTimeout(e))}},[]),(0,t.useEffect)(()=>{if(i&&r&&C&&n){if(g?.legal.requireUsersAcceptTerms&&!n.hasAcceptedTerms){let e=setTimeout(()=>{s("AffirmativeConsentScreen")},d.Q);return()=>clearTimeout(e)}C&&((0,m.s)(n,g.embeddedWallets)?k.current.push(setTimeout(()=>{c({createWallet:{onSuccess:()=>{},onFailure:e=>{console.error(e),x({eventName:"embedded_wallet_creation_failure_logout",payload:{error:e,screen:"FarcasterConnectStatusScreen"}}),a()},callAuthOnSuccessOnClose:!0}}),s("EmbeddedWalletOnAccountCreateScreen")},d.Q)):k.current.push(setTimeout(()=>v({shouldCallAuthOnSuccess:!0,isSuccess:!0}),d.Q)))}},[C,i,r,n]),(0,e.jsx)(j,{connectUri:F,loading:w,success:C,errorMessage:b,onBack:o?l:void 0,onClose:v,onOpenFarcaster:()=>{F&&(window.location.href=F)}})}};let k=a.styled.div`
  margin-top: 24px;
`,A=a.styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 24px;
`,F=a.styled.div`
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 275px;
`,O=a.styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 16px;
`,_=a.styled.div`
  font-size: 0.875rem;
  text-align: center;
  color: var(--privy-color-foreground-2);
`,L=a.styled.div`
  position: relative;
  width: 82px;
  height: 82px;
`},5730,[2,39,2738,8048,5010,5731,5572,8328,5225,5697,2715,2723,2718,4799,5240,5224,5687,5628,2735,2736,1049,6427,5226,5227,5228,5229,2716,2519,2717,7226,2518,2714,2521,2600,2719,2720,1278,2737,2740,2790,4390,4391,4392,4393,2601,8034,4790,2604,4793,4800,4801,4802,4803,4804,5011,5012,2520,2603,5013,5014,5015,8145,5070,5071,2605,5196,5230,5231,5232]);