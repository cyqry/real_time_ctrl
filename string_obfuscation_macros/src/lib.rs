use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use proc_macro::TokenStream;
use quote::quote;
use sha2::{Digest, Sha256};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, ExprLit, ExprMacro, Lit, LitStr, Token};

#[path = "../../common/string_obfuscation_key.rs"]
mod key_material;

struct HiddenInput {
    parts: Punctuated<Expr, Token![,]>,
}

impl Parse for HiddenInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let parts = Punctuated::parse_terminated(input)?;
        if parts.is_empty() {
            return Err(input.error("hidden! 至少需要一个字符串字面量或动态表达式"));
        }
        Ok(Self { parts })
    }
}

/// 在编译期加密静态字符串片段，并在运行到该表达式时解密和拼接。
///
/// 例如 `hidden!("连接失败: ", error)` 中只有 `error` 在运行时求值；中文前缀不会以明文形式
/// 进入目标文件。这个宏是反静态扫描措施，不是秘密存储方案。
#[proc_macro]
pub fn hidden(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as HiddenInput);
    let mut statements = Vec::with_capacity(input.parts.len());
    let mut static_capacity = 0usize;

    for (index, part) in input.parts.into_iter().enumerate() {
        match compile_time_string(&part) {
            Err(error) => return error.to_compile_error().into(),
            Ok(Some(value)) => {
                static_capacity = static_capacity.saturating_add(value.len());
                let (cipher_text, nonce) = encrypt_literal(index, value.as_bytes());
                statements.push(quote! {
                    __hidden_output.push_str(
                        &::common::string_obfuscation::decrypt_literal(
                            &[#(#cipher_text),*],
                            &[#(#nonce),*],
                        )
                    );
                });
            }
            Ok(None) => {
                if let Err(error) = validate_dynamic_expression(&part) {
                    return error.to_compile_error().into();
                }
                statements.push(quote! {
                    if ::core::fmt::write(
                        &mut __hidden_output,
                        ::core::format_args!("{}", #part),
                    ).is_err() {
                        ::std::process::abort();
                    }
                });
            }
        }
    }

    quote! {{
        let mut __hidden_output = ::std::string::String::with_capacity(#static_capacity);
        #(#statements)*
        __hidden_output
    }}
    .into()
}

fn compile_time_string(expression: &Expr) -> syn::Result<Option<String>> {
    let Expr::Lit(ExprLit {
        lit: Lit::Str(value),
        ..
    }) = expression
    else {
        return compile_time_environment(expression);
    };
    Ok(Some(value.value()))
}

/// 支持 `hidden!(env!("NAME"))`：环境值由过程宏在编译期读取并直接加密，最终 PE
/// 只保留密文。该能力用于 build.rs 注入的部署默认值，运行环境覆盖仍在业务代码中处理。
fn compile_time_environment(expression: &Expr) -> syn::Result<Option<String>> {
    let Expr::Macro(ExprMacro { mac, .. }) = expression else {
        return Ok(None);
    };
    if !mac.path.is_ident("env") {
        return Ok(None);
    }

    let name = syn::parse2::<LitStr>(mac.tokens.clone()).map_err(|_| {
        syn::Error::new(
            mac.tokens.span(),
            "hidden! 中的 env! 必须只包含一个环境变量名称字面量",
        )
    })?;
    let value = std::env::var(name.value()).map_err(|_| {
        syn::Error::new(
            name.span(),
            "hidden! 无法读取由 build.rs 注入的编译期环境变量",
        )
    })?;
    Ok(Some(value))
}

fn validate_dynamic_expression(expression: &Expr) -> syn::Result<()> {
    struct LiteralVisitor {
        invalid_span: Option<proc_macro2::Span>,
    }

    impl<'ast> Visit<'ast> for LiteralVisitor {
        fn visit_lit_str(&mut self, value: &'ast syn::LitStr) {
            self.invalid_span.get_or_insert(value.span());
        }

        fn visit_expr_macro(&mut self, expression: &'ast syn::ExprMacro) {
            let is_nested_hidden = expression
                .mac
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "hidden");
            if !is_nested_hidden {
                self.invalid_span.get_or_insert(expression.mac.path.span());
            }
            // 宏 token 无法由 syn 可靠解析成表达式；只允许已知会自行加密字面量的 nested hidden!。
        }

        fn visit_expr(&mut self, expression: &'ast Expr) {
            if self.invalid_span.is_none() {
                visit::visit_expr(self, expression);
            }
        }
    }

    let mut visitor = LiteralVisitor { invalid_span: None };
    visitor.visit_expr(expression);
    match visitor.invalid_span {
        Some(span) => Err(syn::Error::new(
            span,
            "hidden! 的动态参数不能包含字符串字面量或其他宏；请把静态文本拆成独立参数",
        )),
        None => Ok(()),
    }
}

fn encrypt_literal(index: usize, plain_text: &[u8]) -> (Vec<u8>, [u8; 12]) {
    let mut nonce_hasher = Sha256::new();
    nonce_hasher.update(b"real_time_ctrl.hidden.literal.v2\0");
    nonce_hasher.update(index.to_le_bytes());
    nonce_hasher.update(plain_text);
    let digest = nonce_hasher.finalize();
    let mut nonce = [0_u8; 12];
    nonce.copy_from_slice(&digest[..12]);

    let cipher = Aes256Gcm::new_from_slice(&key_material::STRING_OBFUSCATION_KEY)
        .expect("固定的字符串混淆密钥长度必须为 32 字节");
    let cipher_text = cipher
        .encrypt(Nonce::from_slice(&nonce), plain_text)
        .expect("字符串字面量的编译期加密不应失败");
    (cipher_text, nonce)
}
