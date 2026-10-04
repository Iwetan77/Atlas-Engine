__d(function(_g,_r,_i,a,_m,_e,_d){"use strict";function e(e){return e&&e.__esModule?e:{default:e}}Object.defineProperty(_e,'__esModule',{value:!0}),Object.defineProperty(_e,"AwaitingPasswordlessCodeScreen",{enumerable:!0,get:function(){return C}}),Object.defineProperty(_e,"AwaitingPasswordlessCodeScreenView",{enumerable:!0,get:function(){return h}}),Object.defineProperty(_e,"default",{enumerable:!0,get:function(){return C}});var r=_r(_d[0]),t=e(_r(_d[1])),o=e(_r(_d[2])),n=e(_r(_d[3])),i=_r(_d[4]),s=_r(_d[5]),c=_r(_d[6]),l=_r(_d[7]),d=_r(_d[8]),u=_r(_d[9]),p=_r(_d[10]),f=_r(_d[11]),m=_r(_d[12]),v=_r(_d[13]),y=_r(_d[14]);_r(_d[15]),_r(_d[16]),_r(_d[17]),_r(_d[18]),_r(_d[19]),_r(_d[20]),_r(_d[21]),_r(_d[22]),_r(_d[23]),_r(_d[24]),_r(_d[25]),_r(_d[26]),_r(_d[27]),_r(_d[28]),_r(_d[29]),_r(_d[30]),_r(_d[31]),_r(_d[32]),_r(_d[33]),_r(_d[34]),_r(_d[35]),_r(_d[36]),_r(_d[37]),_r(_d[38]),_r(_d[39]),_r(_d[40]),_r(_d[41]),_r(_d[42]),_r(_d[43]),_r(_d[44]),_r(_d[45]),_r(_d[46]),_r(_d[47]),_r(_d[48]),_r(_d[49]),_r(_d[50]),_r(_d[51]),_r(_d[52]),_r(_d[53]),_r(_d[54]),_r(_d[55]),_r(_d[56]),_r(_d[57]),_r(_d[58]),_r(_d[59]),_r(_d[60]),_r(_d[61]),_r(_d[62]),_r(_d[63]),_r(_d[64]),_r(_d[65]),_r(_d[66]),_r(_d[67]);const h=({contactMethod:e,authFlow:c,emailDomain:u,appName:p="Privy",whatsAppEnabled:f=!1,onBack:m,onCodeSubmit:v,onResend:h,errorMessage:E,success:b=!1,resendCountdown:S=0,onInvalidInput:A,onClearError:C})=>{let[I,M]=(0,i.useState)(x);(0,i.useEffect)(()=>{E||M(x)},[E]);let R=async e=>{e.preventDefault();let r=e.currentTarget.value.replace(" ","");if(""===r)return;if(isNaN(Number(r)))return void A?.("Code should be numeric");C?.();let t=Number(e.currentTarget.name?.charAt(5)),o=[...r||[""]].slice(0,g-t),n=[...I.slice(0,t),...o,...I.slice(t+o.length)];M(n);let i=Math.min(Math.max(t+o.length,0),g-1);if(!isNaN(Number(e.currentTarget.value))){let e=document.querySelector(`input[name=code-${i}]`);e?.focus()}if(n.every(e=>e&&!isNaN(+e))){let e=document.querySelector(`input[name=code-${i}]`);e?.blur(),await(v?.(n.join("")))}};return(0,r.jsx)(y.S,{title:"Enter confirmation code",subtitle:(0,r.jsxs)("span","email"===c?{children:["Please check ",(0,r.jsx)(k,{children:e})," for an email from"," ",u??"privy.io"," and enter your code below."]}:{children:["Please check ",(0,r.jsx)(k,{children:e})," for a",f?" WhatsApp":""," message from ",p," and enter your code below."]}),icon:"email"===c?o.default:n.default,onBack:m,showBack:!0,helpText:(0,r.jsxs)(N,{children:[(0,r.jsxs)("span",{children:["Didn't get ","email"===c?"an email":"a message","?"]}),S?(0,r.jsxs)(j,{children:[(0,r.jsx)(t.default,{color:"var(--privy-color-foreground)",strokeWidth:1.33,height:"12px",width:"12px"}),(0,r.jsx)("span",{children:"Code sent"})]}):(0,r.jsx)(d.L,{as:"button",size:"sm",onClick:h,children:"Resend code"})]}),children:(0,r.jsx)(_,{children:(0,r.jsx)(l.H,{children:(0,r.jsxs)(w,{children:[(0,r.jsx)("div",{children:I.map((e,t)=>(0,r.jsx)("input",{name:`code-${t}`,type:"text",value:I[t],onChange:R,onKeyUp:e=>{"Backspace"===e.key&&(e=>{if(C?.(),M([...I.slice(0,e),"",...I.slice(e+1)]),e>0){let r=document.querySelector(`input[name=code-${e-1}]`);r?.focus()}})(t)},inputMode:"numeric",autoFocus:0===t,pattern:"[0-9]",className:`${b?"success":""} ${E?"fail":""}`,autoComplete:s.isMobile?"one-time-code":"off"},t))}),(0,r.jsx)(T,{$fail:!!E,$success:b,children:(0,r.jsx)("span",{children:"Invalid or expired verification code"===E?"Incorrect code":E||(b?"Success!":"")})})]})})})})};let g=6,x=Array(6).fill("");var E,b,S=((E=S||{})[E.RESET_AFTER_DELAY=0]="RESET_AFTER_DELAY",E[E.CLEAR_ON_NEXT_VALID_INPUT=1]="CLEAR_ON_NEXT_VALID_INPUT",E),A=((b=A||{})[b.EMAIL=0]="EMAIL",b[b.SMS=1]="SMS",b);const C={component:()=>{let{navigate:e,lastScreen:t,navigateBack:o,setModalData:n,onUserCloseViaDialogOrKeybindRef:s}=(0,m.u)(),c=(0,u.a)(),{closePrivyModal:l,resendEmailCode:d,resendSmsCode:y,getAuthMeta:g,loginWithCode:x,updateWallets:E,createAnalyticsEvent:b}=(0,f.u)(),{authenticated:S,logout:A,user:C}=(0,u.u)(),{whatsAppEnabled:_}=(0,u.a)(),[w,T]=(0,i.useState)(!1),[N,j]=(0,i.useState)(null),[k,I]=(0,i.useState)(null),[M,R]=(0,i.useState)(0);s.current=()=>null;let O=g()?.email?0:1,D=0===O?g()?.email||"":g()?.phoneNumber||"",L=u.Q-500;return(0,i.useEffect)(()=>{if(M){let e=setTimeout(()=>{R(M-1)},1e3);return()=>clearTimeout(e)}},[M]),(0,i.useEffect)(()=>{if(S&&w&&C){if(c?.legal.requireUsersAcceptTerms&&!C.hasAcceptedTerms){let r=setTimeout(()=>{e("AffirmativeConsentScreen")},L);return()=>clearTimeout(r)}if((0,v.s)(C,c.embeddedWallets)){let r=setTimeout(()=>{n({createWallet:{onSuccess:()=>{},onFailure:e=>{console.error(e),b({eventName:"embedded_wallet_creation_failure_logout",payload:{error:e,screen:"AwaitingPasswordlessCodeScreen"}}),A()},callAuthOnSuccessOnClose:!0}}),e("EmbeddedWalletOnAccountCreateScreen")},L);return()=>clearTimeout(r)}{E();let e=setTimeout(()=>l({shouldCallAuthOnSuccess:!0,isSuccess:!0}),u.Q);return()=>clearTimeout(e)}}},[S,w,C]),(0,i.useEffect)(()=>{if(N&&0===k){let e=setTimeout(()=>{j(null),I(null);let e=document.querySelector("input[name=code-0]");e?.focus()},1400);return()=>clearTimeout(e)}},[N,k]),(0,r.jsx)(h,{contactMethod:D,authFlow:0===O?"email":"sms",emailDomain:c?.appearance.emailDomain,appName:c?.name,whatsAppEnabled:_,onBack:()=>o(),onCodeSubmit:async r=>{try{await x(r),T(!0)}catch(r){if(r instanceof p.c&&r.privyErrorCode===p.a.INVALID_CREDENTIALS)j("Invalid or expired verification code"),I(0);else if(r instanceof p.c&&r.privyErrorCode===p.a.CANNOT_LINK_MORE_OF_TYPE)j(r.message);else{if(r instanceof p.c&&r.privyErrorCode===p.a.USER_LIMIT_REACHED)return console.error(new p.j(r).toString()),void e("UserLimitReachedScreen");if(r instanceof p.c&&r.privyErrorCode===p.a.USER_DOES_NOT_EXIST)return void e("AccountNotFoundScreen");if(r instanceof p.c&&r.privyErrorCode===p.a.LINKED_TO_ANOTHER_USER)return n({errorModalData:{error:r,previousScreen:t??"AwaitingPasswordlessCodeScreen"}}),void e("ErrorScreen",!1);if(r instanceof p.c&&r.privyErrorCode===p.a.DISALLOWED_PLUS_EMAIL)return n({inlineError:{error:r}}),void e("ConnectOrCreateScreen",!1);if(r instanceof p.c&&r.privyErrorCode===p.a.ACCOUNT_TRANSFER_REQUIRED&&r.data?.data?.nonce)return n({accountTransfer:{nonce:r.data?.data?.nonce,account:D,displayName:r.data?.data?.account?.displayName,linkMethod:0===O?"email":"sms",embeddedWalletAddress:r.data?.data?.otherUser?.embeddedWalletAddress}}),void e("LinkConflictScreen");j("Issue verifying code"),I(0)}}},onResend:async()=>{R(30),0===O?await d():await y()},errorMessage:N||void 0,success:w,resendCountdown:M,onInvalidInput:e=>{j(e),I(1)},onClearError:()=>{1===k&&(j(null),I(null))}})}};let _=c.styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  margin: auto;
  gap: 16px;
  flex-grow: 1;
  width: 100%;
`,w=c.styled.div`
  display: flex;
  flex-direction: column;
  width: 100%;
  gap: 12px;

  > div:first-child {
    display: flex;
    justify-content: center;
    gap: 0.5rem;
    width: 100%;
    border-radius: var(--privy-border-radius-sm);

    > input {
      border: 1px solid var(--privy-color-foreground-4);
      background: var(--privy-color-background);
      border-radius: var(--privy-border-radius-sm);
      padding: 8px 10px;
      height: 48px;
      width: 40px;
      text-align: center;
      font-size: 18px;
      font-weight: 600;
      color: var(--privy-color-foreground);
      transition: all 0.2s ease;
    }

    > input:focus {
      border: 1px solid var(--privy-color-foreground);
      box-shadow: 0 0 0 1px var(--privy-color-foreground);
    }

    > input:invalid {
      border: 1px solid var(--privy-color-error);
    }

    > input.success {
      border: 1px solid var(--privy-color-border-success);
      background: var(--privy-color-success-bg);
    }

    > input.fail {
      border: 1px solid var(--privy-color-border-error);
      background: var(--privy-color-error-bg);
      animation: shake 180ms;
      animation-iteration-count: 2;
    }
  }

  @keyframes shake {
    0% {
      transform: translate(1px, 0);
    }
    33% {
      transform: translate(-1px, 0);
    }
    67% {
      transform: translate(-1px, 0);
    }
    100% {
      transform: translate(1px, 0);
    }
  }
`,T=c.styled.div`
  line-height: 20px;
  min-height: 20px;
  font-size: 14px;
  font-weight: 400;
  color: ${e=>e.$success?"var(--privy-color-success-dark)":e.$fail?"var(--privy-color-error-dark)":"transparent"};
  display: flex;
  justify-content: center;
  width: 100%;
  text-align: center;
`,N=c.styled.div`
  display: flex;
  gap: 8px;
  align-items: center;
  justify-content: center;
  width: 100%;
  color: var(--privy-color-foreground-2);
`,j=c.styled.div`
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: var(--privy-border-radius-sm);
  padding: 2px 8px;
  gap: 4px;
  background: var(--privy-color-background-2);
  color: var(--privy-color-foreground-2);
`,k=c.styled.span`
  font-weight: 500;
  word-break: break-all;
  color: var(--privy-color-foreground);
`},5675,[2,5676,5677,5678,39,2743,8095,5641,5576,2720,2728,2723,4804,5245,5229,2721,2524,2722,7273,2523,2719,2526,2605,2724,2725,1051,1280,2740,2741,6474,2742,2745,2795,4395,4396,4397,4398,2606,8081,4795,2609,4798,4805,4806,4807,4808,4809,5015,5016,5017,2525,2608,5018,5019,5020,8192,5075,5076,2610,5201,5230,5231,5232,5233,5234,5235,5236,5237]);
__d(function(g,r,i,a,m,e,d){const t=r(d[0]);function l({title:l,titleId:n,...o},c){return t.createElement("svg",Object.assign({xmlns:"http://www.w3.org/2000/svg",viewBox:"0 0 20 20",fill:"currentColor","aria-hidden":"true","data-slot":"icon",ref:c,"aria-labelledby":n},o),l?t.createElement("title",{id:n},l):null,t.createElement("path",{fillRule:"evenodd",d:"M16.704 4.153a.75.75 0 0 1 .143 1.052l-8 10.5a.75.75 0 0 1-1.127.075l-4.5-4.5a.75.75 0 0 1 1.06-1.06l3.894 3.893 7.48-9.817a.75.75 0 0 1 1.05-.143Z",clipRule:"evenodd"}))}const n=t.forwardRef(l);m.exports=n},5676,[39]);