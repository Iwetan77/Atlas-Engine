__d(function(_g,_r,_i,_a,_m,_e,_d){"use strict";Object.defineProperty(_e,'__esModule',{value:!0}),Object.defineProperty(_e,"FarcasterSignerStatusScreen",{enumerable:!0,get:function(){return S}}),Object.defineProperty(_e,"FarcasterSignerStatusView",{enumerable:!0,get:function(){return m}}),Object.defineProperty(_e,"default",{enumerable:!0,get:function(){return S}});var e=_r(_d[0]),t=_r(_d[1]),a=_r(_d[2]),r=_r(_d[3]),s=_r(_d[4]),n=_r(_d[5]),i=_r(_d[6]),l=_r(_d[7]),o=_r(_d[8]),c=_r(_d[9]),d=_r(_d[10]),u=_r(_d[11]),p=_r(_d[12]);_r(_d[13]),_r(_d[14]),_r(_d[15]),_r(_d[16]),_r(_d[17]),_r(_d[18]),_r(_d[19]),_r(_d[20]),_r(_d[21]),_r(_d[22]),_r(_d[23]),_r(_d[24]),_r(_d[25]),_r(_d[26]),_r(_d[27]),_r(_d[28]),_r(_d[29]),_r(_d[30]);let g="#8a63d2";const m=({appName:t,loading:r,success:o,errorMessage:c,connectUri:d,onBack:m,onClose:S,onOpenFarcaster:w})=>(0,e.jsx)(u.S,a.isMobile||r?a.isIOS?{title:c?c.message:"Add a signer to Farcaster",subtitle:c?c.detail:`This will allow ${t} to add casts, likes, follows, and more on your behalf.`,icon:p.F,iconVariant:"loading",iconLoadingStatus:{success:o,fail:!!c},primaryCta:d&&w?{label:"Open Farcaster app",onClick:w}:void 0,onBack:m,onClose:S,watermark:!0}:{title:c?c.message:"Requesting signer from Farcaster",subtitle:c?c.detail:"This should only take a moment",icon:p.F,iconVariant:"loading",iconLoadingStatus:{success:o,fail:!!c},onBack:m,onClose:S,watermark:!0,children:d&&a.isMobile&&(0,e.jsx)(f,{children:(0,e.jsx)(i.O,{text:"Take me to Farcaster",url:d,color:g})})}:{title:"Add a signer to Farcaster",subtitle:`This will allow ${t} to add casts, likes, follows, and more on your behalf.`,onBack:m,onClose:S,watermark:!0,children:(0,e.jsxs)(h,{children:[(0,e.jsx)(v,{children:d?(0,e.jsx)(l.Q,{url:d,size:275,squareLogoElement:p.F}):(0,e.jsx)(b,{children:(0,e.jsx)(n.L,{})})}),(0,e.jsxs)(y,{children:[(0,e.jsx)(x,{children:"Or copy this link and paste it into a phone browser to open the Farcaster app."}),d&&(0,e.jsx)(s.C,{text:d,itemName:"link",color:g})]})]})});let f=r.styled.div`
  margin-top: 24px;
`,h=r.styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 24px;
`,v=r.styled.div`
  padding: 24px;
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 275px;
`,y=r.styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 16px;
`,x=r.styled.div`
  font-size: 0.875rem;
  text-align: center;
  color: var(--privy-color-foreground-2);
`,b=r.styled.div`
  position: relative;
  width: 82px;
  height: 82px;
`;const S={component:()=>{let{lastScreen:a,navigateBack:r,data:s}=(0,d.u)(),n=(0,o.a)(),{requestFarcasterSignerStatus:i,closePrivyModal:l}=(0,c.u)(),[u,p]=(0,t.useState)(void 0),[g,f]=(0,t.useState)(!1),[h,v]=(0,t.useState)(!1),y=(0,t.useRef)([]),x=s?.farcasterSigner;(0,t.useEffect)(()=>{let e=Date.now(),t=setInterval(async()=>{if(!x?.public_key)return clearInterval(t),void p({retryable:!0,message:"Connect failed",detail:"Something went wrong. Please try again."});"approved"===x.status&&(clearInterval(t),f(!1),v(!0),y.current.push(setTimeout(()=>l({shouldCallAuthOnSuccess:!1,isSuccess:!0}),o.Q)));let a=await i(x?.public_key),r=Date.now()-e;"approved"===a.status?(clearInterval(t),f(!1),v(!0),y.current.push(setTimeout(()=>l({shouldCallAuthOnSuccess:!1,isSuccess:!0}),o.Q))):r>3e5?(clearInterval(t),p({retryable:!0,message:"Connect failed",detail:"The request timed out. Try again."})):"revoked"===a.status&&(clearInterval(t),p({retryable:!0,message:"Request rejected",detail:"The request was rejected. Please try again."}))},2e3);return()=>{clearInterval(t),y.current.forEach(e=>clearTimeout(e))}},[]);let b="pending_approval"===x?.status?x.signer_approval_url:void 0;return(0,e.jsx)(m,{appName:n.name,loading:g,success:h,errorMessage:u,connectUri:b,onBack:a?r:void 0,onClose:l,onOpenFarcaster:()=>{b&&(window.location.href=b)}})}}},4338,[11,18,2414,3649,4497,3881,4496,4423,2398,2401,3663,4403,4469,4163,2411,2406,2407,2412,989,1400,2399,2203,2400,4404,4405,4406,4407,4408,4409,4410,4411]);