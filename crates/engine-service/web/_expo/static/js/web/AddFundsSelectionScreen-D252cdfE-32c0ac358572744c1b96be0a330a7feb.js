__d(function(g,_r,i,a,_m,e,d){"use strict";Object.defineProperty(e,'__esModule',{value:!0}),Object.defineProperty(e,"AddFundsSelectionScreen",{enumerable:!0,get:function(){return m}}),Object.defineProperty(e,"default",{enumerable:!0,get:function(){return m}});var r=_r(d[0]),t=_r(d[1]),n=_r(d[2]),s=_r(d[3]),l=_r(d[4]),c=_r(d[5]),o=_r(d[6]),u=_r(d[7]),h=_r(d[8]),x=_r(d[9]),j=_r(d[10]),y=_r(d[11]),f=_r(d[12]);_r(d[13]),_r(d[14]),_r(d[15]),_r(d[16]),_r(d[17]),_r(d[18]),_r(d[19]),_r(d[20]),_r(d[21]),_r(d[22]),_r(d[23]),_r(d[24]),_r(d[25]),_r(d[26]),_r(d[27]),_r(d[28]),_r(d[29]),_r(d[30]),_r(d[31]),_r(d[32]),_r(d[33]),_r(d[34]),_r(d[35]),_r(d[36]),_r(d[37]),_r(d[38]),_r(d[39]),_r(d[40]),_r(d[41]),_r(d[42]),_r(d[43]),_r(d[44]),_r(d[45]),_r(d[46]),_r(d[47]);const m={component:()=>{let s=(0,f.b)(),{onUserCloseViaDialogOrKeybindRef:m}=(0,x.u)(),b=(0,u.a)(),v=(0,n.useRef)(!1),O=(0,h.u)(h.i),A=(0,h.u)(h.a),[k,E]=(0,n.useState)(!1),_=O?"APPLE_PAY":!1===O&&A?"GOOGLE_PAY":null,L=!0===O||!1===O&&void 0!==A,F=!s?.startFiat||L||k;(0,n.useEffect)(()=>{let r=window.setTimeout(()=>E(!0),2e3);return()=>window.clearTimeout(r)},[]),(0,n.useEffect)(()=>{s&&(v.current=!1)},[s]);let G=(0,n.useRef)(null);(0,n.useEffect)(()=>{s&&!s.error&&F&&G.current!==s&&(G.current=s,s.recordRowsViewed?.({walletPay:s.startFiat?_:void 0,walletPayTimedOut:s.startFiat?!L:void 0}))},[F,s,_,L]);let D=(0,n.useCallback)(async()=>{!v.current&&s&&(v.current=!0,(0,f.r)(),await s.onCancel())},[s]);if((0,n.useEffect)(()=>(m.current=D,()=>{m.current===D&&(m.current=null)}),[D,m]),!s)return null;if(s.error)return(0,r.jsx)(y.C,{title:"Unable to add funds",subtitle:s.error,showClose:!0,onClose:D,primaryCta:{label:"Close",onClick:D}});let R=async r=>{v.current||(v.current=!0,await(s.startFiat?.(r)))};return(0,r.jsx)(y.C,{title:"Pay with",subtitle:"Debit cards typically have higher success rates than credit cards, even with Apple Pay or Google Pay.",showClose:!0,onClose:D,children:F?(0,r.jsxs)(j.S,{style:{marginTop:"1rem"},$colorScheme:b.appearance.palette.colorScheme,children:[s.startFiat&&(0,r.jsxs)(y.O,{onClick:()=>R("CREDIT_DEBIT_CARD"),children:[(0,r.jsx)(C,{children:(0,r.jsx)(t.CreditCard,{})}),(0,r.jsxs)(P,{children:[(0,r.jsx)(y.a,{children:"Debit or credit card"}),(0,r.jsx)(w,{children:"Less than 10 minutes"})]})]}),s.startFiat&&"APPLE_PAY"===_&&(0,r.jsxs)(y.O,{onClick:()=>R("APPLE_PAY"),children:[(0,r.jsx)(C,{children:(0,r.jsx)(c.A,{width:18,height:18})}),(0,r.jsxs)(P,{children:[(0,r.jsx)(y.a,{children:"Apple Pay"}),(0,r.jsx)(w,{children:"Less than 10 minutes"})]})]}),s.startFiat&&"GOOGLE_PAY"===_&&(0,r.jsxs)(y.O,{onClick:()=>R("GOOGLE_PAY"),children:[(0,r.jsx)(C,{children:(0,r.jsx)(o.G,{width:18,height:18})}),(0,r.jsxs)(P,{children:[(0,r.jsx)(y.a,{children:"Google Pay"}),(0,r.jsx)(w,{children:"Less than 10 minutes"})]})]}),s.startFiat&&(0,r.jsxs)(y.O,{onClick:()=>R("BANK"),children:[(0,r.jsx)(C,{children:(0,r.jsx)(t.Landmark,{})}),(0,r.jsxs)(P,{children:[(0,r.jsx)(y.a,{children:"Bank account"}),(0,r.jsx)(w,{children:"1\u20132 days"})]})]}),s.startCrypto&&(0,r.jsxs)(y.O,{onClick:async()=>{v.current||(v.current=!0,await(s.startCrypto?.()))},children:[(0,r.jsx)(C,{children:(0,r.jsx)(t.Wallet,{})}),(0,r.jsxs)(P,{children:[(0,r.jsx)(y.a,{children:"Crypto wallet or exchange"}),(0,r.jsx)(w,{children:"Instant"})]})]})]}):(0,r.jsx)(p,{children:(0,r.jsx)(l.N,{size:"50px"})})})}};let p=s.styled.div`
  display: flex;
  justify-content: center;
  align-items: center;
  margin-top: 1rem;
  min-height: 8rem;
`,C=s.styled.span`
  width: 2rem;
  height: 2rem;
  border-radius: var(--privy-border-radius-full);
  background-color: var(--privy-color-background-2);
  color: var(--privy-color-icon-muted);
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  overflow: hidden;

  svg {
    width: 1.125rem;
    height: 1.125rem;
  }
`,P=s.styled.span`
  display: flex;
  flex-direction: column;
  align-items: flex-start;
`,w=s.styled.span`
  font-size: 0.875rem;
  line-height: 1.25rem;
  color: var(--privy-color-foreground-3);
`},5235,[2,8335,39,8055,5234,5236,5237,2717,5238,4801,5239,5240,5014,2718,2521,2719,2720,5012,5226,5227,5228,5229,5230,5231,5232,5233,2522,2606,2605,2722,2725,7233,5015,4803,1280,4804,5016,4393,4395,2523,2737,2738,1051,6434,2602,5017,8152,4394]);