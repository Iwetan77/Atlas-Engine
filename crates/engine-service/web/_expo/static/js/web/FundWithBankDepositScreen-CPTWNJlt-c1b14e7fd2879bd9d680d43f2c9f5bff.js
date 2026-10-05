__d(function(_g,_r,_i,_a,_m,_e,_d){"use strict";Object.defineProperty(_e,'__esModule',{value:!0}),Object.defineProperty(_e,"FundWithBankDepositScreen",{enumerable:!0,get:function(){return B}}),Object.defineProperty(_e,"default",{enumerable:!0,get:function(){return B}});var e=_r(_d[0]),t=_r(_d[1]),s=_r(_d[2]),r=_r(_d[3]),o=_r(_d[4]),a=_r(_d[5]),n=_r(_d[6]),i=_r(_d[7]),l=_r(_d[8]),c=_r(_d[9]),u=_r(_d[10]),d=_r(_d[11]),p=_r(_d[12]),m=_r(_d[13]);_r(_d[14]),_r(_d[15]),_r(_d[16]),_r(_d[17]),_r(_d[18]),_r(_d[19]),_r(_d[20]),_r(_d[21]),_r(_d[22]),_r(_d[23]),_r(_d[24]),_r(_d[25]),_r(_d[26]),_r(_d[27]),_r(_d[28]),_r(_d[29]),_r(_d[30]),_r(_d[31]),_r(_d[32]),_r(_d[33]),_r(_d[34]),_r(_d[35]),_r(_d[36]),_r(_d[37]),_r(_d[38]),_r(_d[39]),_r(_d[40]),_r(_d[41]),_r(_d[42]),_r(_d[43]),_r(_d[44]),_r(_d[45]),_r(_d[46]),_r(_d[47]);const y=e=>{try{return e.location.origin}catch{return}},f=({data:t,onClose:s})=>(0,e.jsx)(u.S,{showClose:!0,onClose:s,title:"Initiate bank transfer",subtitle:"Use the details below to complete a bank transfer from your bank.",primaryCta:{label:"Done",onClick:s},watermark:!1,footerText:"Exchange rates and fees are set when you authorize and determine the amount you receive. You'll see the applicable rates and fees for your transaction separately",children:(0,e.jsx)(g,{children:(c.D[t.deposit_instructions.asset]||[]).map(([s,r],o)=>{let a=t.deposit_instructions[s];if(!a||Array.isArray(a))return null;let i="asset"===s?a.toUpperCase():a,c=i.length>100?`${i.slice(0,9)}...${i.slice(-9)}`:i;return(0,e.jsxs)(h,{children:[(0,e.jsx)(k,{children:r}),(0,e.jsx)(l.a,{value:i,includeChildren:n.isMobile,children:(0,e.jsx)(C,{children:c})})]},o)})})});let g=i.styled.ol`
  border-color: var(--privy-color-border-default);
  border-width: 1px;
  border-radius: var(--privy-border-radius-mdlg);
  border-style: solid;
  display: flex;
  flex-direction: column;

  && {
    padding: 0 1rem;
  }
`,h=i.styled.li`
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 1rem 0;

  &:not(:first-of-type) {
    border-top: 1px solid var(--privy-color-border-default);
  }

  & > {
    :nth-child(1) {
      flex-basis: 30%;
    }

    :nth-child(2) {
      flex-basis: 60%;
    }
  }
`,k=i.styled.span`
  color: var(--privy-color-foreground);
  font-kerning: none;
  font-variant-numeric: lining-nums proportional-nums;
  font-feature-settings: 'calt' off;

  /* text-xs/font-regular */
  font-size: 0.75rem;
  font-style: normal;
  font-weight: 400;
  line-height: 1.125rem; /* 150% */

  text-align: left;
  flex-shrink: 0;
`,C=i.styled.span`
  color: var(--privy-color-foreground);
  font-kerning: none;
  font-feature-settings: 'calt' off;

  /* text-sm/font-medium */
  font-size: 0.875rem;
  font-style: normal;
  font-weight: 500;
  line-height: 1.375rem; /* 157.143% */

  text-align: right;
  word-break: break-all;
`;const b=({onClose:t})=>(0,e.jsx)(u.S,{showClose:!0,onClose:t,icon:d.XCircle,iconVariant:"error",title:"Something went wrong",subtitle:"We couldn't complete account setup. This isn't caused by anything you did.",primaryCta:{label:"Close",onClick:t},watermark:!0}),w=({onClose:t,reason:s})=>{let r=s?s.charAt(0).toLowerCase()+s.slice(1):void 0;return(0,e.jsx)(u.S,{showClose:!0,onClose:t,icon:d.XCircle,iconVariant:"error",title:"Identity verification failed",subtitle:r?`We can't complete identity verification because ${r}. Please try again or contact support for assistance.`:"We couldn't verify your identity. Please try again or contact support for assistance.",primaryCta:{label:"Close",onClick:t},watermark:!0})},v=({onClose:t,email:s})=>(0,e.jsx)(u.S,{showClose:!0,onClose:t,icon:d.Hourglass,title:"Identity verification in progress",subtitle:"We're waiting for Persona to approve your identity verification. This usually takes a few minutes, but may take up to 24 hours.",primaryCta:{label:"Done",onClick:t},watermark:!0,children:(0,e.jsxs)(p.I,{theme:"light",children:["You'll receive an email at ",s," once approved with instructions for completing your deposit."]})}),x=({onClose:t,onAcceptTerms:s,isLoading:r})=>(0,e.jsx)(u.S,{showClose:!0,onClose:t,icon:d.UserCheck,title:"Verify your identity to continue",subtitle:"Finish verification with Persona \u2014 it takes just a few minutes and requires a government ID.",helpText:(0,e.jsxs)(e.Fragment,{children:['This app uses Bridge to securely connect accounts and move funds. By clicking "Accept," you agree to Bridge\'s'," ",(0,e.jsx)("a",{href:"https://www.bridge.xyz/legal",target:"_blank",rel:"noopener noreferrer",children:"Terms of Service"})," ","and"," ",(0,e.jsx)("a",{href:"https://www.bridge.xyz/legal/row-privacy-policy/bridge-building-limited",target:"_blank",rel:"noopener noreferrer",children:"Privacy Policy"}),"."]}),primaryCta:{label:"Accept and continue",onClick:s,loading:r},watermark:!0}),j=({onClose:t})=>(0,e.jsx)(u.S,{showClose:!0,onClose:t,icon:d.Check,iconVariant:"success",title:"Identity verified successfully",subtitle:"We've successfully verified your identity. Now initiate a bank transfer to view instructions.",primaryCta:{label:"Initiate bank transfer",onClick:()=>{},loading:!0},watermark:!0}),S=({opts:t,onClose:s,onBack:r,onEditSourceAsset:o,onSelectAmount:a,isLoading:n})=>(0,e.jsxs)(u.S,{showClose:!0,onClose:s,showBack:!!r,onBack:r,headerTitle:`Buy ${t.destination.asset.toLocaleUpperCase()}`,primaryCta:{label:"Continue",onClick:a,loading:n},watermark:!0,children:[(0,e.jsx)(m.A,{currency:t.source.selectedAsset,inputMode:"decimal",autoFocus:!0}),(0,e.jsx)(m.C,{selectedAsset:t.source.selectedAsset,onEditSourceAsset:o})]}),A=({onClose:t,onBack:s,onAcceptTerms:r,onSelectAmount:o,onSelectSource:a,onEditSourceAsset:n,opts:i,state:l,email:c,isLoading:u})=>"select-amount"===l.status?(0,e.jsx)(S,{onClose:t,onBack:s,onSelectAmount:o,onEditSourceAsset:n,opts:i,isLoading:u}):"select-source-asset"===l.status?(0,e.jsx)(m.S,{onSelectSource:a,opts:i,isLoading:u}):"kyc-prompt"===l.status?(0,e.jsx)(x,{onClose:t,onAcceptTerms:r,opts:i,isLoading:u}):"kyc-incomplete"===l.status?(0,e.jsx)(v,{onClose:t,email:c}):"kyc-success"===l.status?(0,e.jsx)(j,{onClose:t}):"kyc-error"===l.status?(0,e.jsx)(w,{onClose:t,reason:l.reason}):"account-details"===l.status?(0,e.jsx)(f,{onClose:t,data:l.data}):"create-customer-error"===l.status||"get-customer-error"===l.status?(0,e.jsx)(b,{onClose:t}):null,B={component:()=>{let{user:n}=(0,a.u)(),i=(0,o.u)().data;if(!i?.FundWithBankDepositScreen)throw Error("Missing data");let{onSuccess:l,onFailure:c,onBack:u,opts:d,createOrUpdateCustomer:p,getCustomer:m,getOrCreateVirtualAccount:f}=i.FundWithBankDepositScreen,[g,h]=(0,t.useState)(d),[k,C]=(0,t.useState)({status:"select-amount"}),[b,w]=(0,t.useState)(null),[v,x]=(0,t.useState)(!1),j=(0,t.useRef)(null),S=(0,t.useCallback)(async()=>{let e;x(!0),w(null);try{e=await m({kycRedirectUrl:window.location.origin})}catch(e){if(!e||"object"!=typeof e||!("status"in e)||404!==e.status)return C({status:"get-customer-error"}),w(e),void x(!1)}if(!e)try{e=await p({hasAcceptedTerms:!1,kycRedirectUrl:window.location.origin})}catch(e){return C({status:"create-customer-error"}),w(e),void x(!1)}if(!e)return C({status:"create-customer-error"}),w(Error("Unable to create customer")),void x(!1);if("not_started"===e.status&&e.kyc_url)return C({status:"kyc-prompt",kycUrl:e.kyc_url}),void x(!1);if("not_started"===e.status)return C({status:"get-customer-error"}),w(Error("Unexpected user state")),void x(!1);if("rejected"===e.status)return C({status:"kyc-error",reason:e.rejection_reasons?.[0]?.reason}),w(Error("User KYC rejected.")),void x(!1);if("incomplete"===e.status)return C({status:"kyc-incomplete"}),void x(!1);if("active"!==e.status)return C({status:"get-customer-error"}),w(Error("Unexpected user state")),void x(!1);e.status;try{let e=await f({destination:g.destination,provider:g.provider,source:{asset:g.source.selectedAsset}});C({status:"account-details",data:e})}catch(e){return C({status:"create-customer-error"}),w(e),void x(!1)}},[g]),B=(0,t.useCallback)(async()=>{if(w(null),x(!0),"kyc-prompt"!==k.status)return w(Error("Unexpected state")),void x(!1);let e=(0,r.trigger)({location:k.kycUrl});if(await p({hasAcceptedTerms:!0}),!e)return w(Error("Unable to begin kyc flow.")),x(!1),void C({status:"create-customer-error"});j.current=new AbortController;let t=await(async(e,t)=>{let r=await(0,s.poll)({operation:async()=>({done:y(e)===window.location.origin,closed:e.closed}),until:({done:e,closed:t})=>e||t,delay:0,interval:500,attempts:360,signal:t});return"aborted"===r.status?(e.close(),{status:"aborted"}):"max_attempts"===r.status?{status:"timeout"}:r.result.done?(e.close(),{status:"redirected"}):{status:"closed"}})(e,j.current.signal);if("aborted"===t.status)return;if("closed"===t.status)return void x(!1);t.status;let o=await(0,s.poll)({operation:()=>m({}),until:e=>"active"===e.status||"rejected"===e.status,delay:0,interval:2e3,attempts:60,signal:j.current.signal});if("aborted"!==o.status){if("max_attempts"===o.status)return C({status:"kyc-incomplete"}),void x(!1);if(o.status,"rejected"===o.result.status)return C({status:"kyc-error",reason:o.result.rejection_reasons?.[0]?.reason}),w(Error("User KYC rejected.")),void x(!1);if("active"!==o.result.status)return C({status:"kyc-incomplete"}),void x(!1);e.closed||e.close(),o.result.status;try{C({status:"kyc-success"});let e=await f({destination:g.destination,provider:g.provider,source:{asset:g.source.selectedAsset}});C({status:"account-details",data:e})}catch(e){C({status:"create-customer-error"}),w(e)}finally{x(!1)}}},[C,w,x,p,f,k,g,j]),U=(0,t.useCallback)(e=>{C({status:"select-amount"}),h({...g,source:{...g.source,selectedAsset:e}})},[C,h]),_=(0,t.useCallback)(()=>{C({status:"select-source-asset"})},[C]);return(0,e.jsx)(A,{onClose:(0,t.useCallback)(async()=>{j.current?.abort(),!g.showBackButton||"select-amount"!==k.status&&"select-source-asset"!==k.status?b?c(b):await l():c(Error("User cancelled funding"))},[b,j,c,l,g.showBackButton,k.status]),onBack:u,opts:g,state:k,isLoading:v,email:n.email.address,onAcceptTerms:B,onSelectAmount:S,onSelectSource:U,onEditSourceAsset:_})}}},4398,[11,18,2217,3899,3677,2412,2428,3663,4478,3897,4460,4459,4760,4759,2415,2413,2414,2218,2301,2300,2417,2420,2421,3898,3679,1212,3680,3629,3631,2219,2425,2426,1000,1411,993,3900,3901,3630,4461,3895,4462,4463,4464,4465,4466,4467,4468,4761]);