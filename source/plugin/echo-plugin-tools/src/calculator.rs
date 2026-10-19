//! Calculator tool: safe evaluation of arithmetic expressions.
//!
//! Implements a small recursive-descent parser for
//! `+ - * / ( )` and numbers — no `eval`, no dependencies.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolError};

#[derive(Debug, Default)]
pub struct CalculatorTool;

#[async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "计算一个算术表达式，支持 + - * / ( ) 和数字。参数: expression(例如 \"(2+3)*4\")。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expression": {"type": "string", "description": "要计算的算术表达式"}
            },
            "required": ["expression"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        let expr = arguments
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("缺少 expression".into()))?;
        match evaluate(expr) {
            Ok(v) => Ok(v.to_string()),
            Err(e) => Err(ToolError::InvalidArguments(e)),
        }
    }
}

pub fn evaluate(expr: &str) -> Result<f64, String> {
    let tokens = tokenize(expr)?;
    let mut parser = Parser { tokens, pos: 0 };
    let value = parser.parse_expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!("表达式多余部分: {:?}", parser.tokens[parser.pos]));
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
}

fn tokenize(expr: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = expr.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' => {
                chars.next();
            }
            '+' => {
                tokens.push(Token::Plus);
                chars.next();
            }
            '-' => {
                tokens.push(Token::Minus);
                chars.next();
            }
            '*' => {
                tokens.push(Token::Star);
                chars.next();
            }
            '/' => {
                tokens.push(Token::Slash);
                chars.next();
            }
            '(' => {
                tokens.push(Token::LParen);
                chars.next();
            }
            ')' => {
                tokens.push(Token::RParen);
                chars.next();
            }
            '0'..='9' | '.' => {
                let mut num = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() || d == '.' {
                        num.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let v: f64 = num.parse().map_err(|_| format!("无效数字: {num}"))?;
                tokens.push(Token::Num(v));
            }
            other => return Err(format!("无法识别的字符: {other}")),
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// expr := term (('+'|'-') term)*
    fn parse_expr(&mut self) -> Result<f64, String> {
        let mut value = self.parse_term()?;
        loop {
            match self.peek() {
                Some(Token::Plus) => {
                    self.next();
                    value += self.parse_term()?;
                }
                Some(Token::Minus) => {
                    self.next();
                    value -= self.parse_term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    /// term := factor (('*'|'/') factor)*
    fn parse_term(&mut self) -> Result<f64, String> {
        let mut value = self.parse_factor()?;
        loop {
            match self.peek() {
                Some(Token::Star) => {
                    self.next();
                    value *= self.parse_factor()?;
                }
                Some(Token::Slash) => {
                    self.next();
                    let divisor = self.parse_factor()?;
                    if divisor == 0.0 {
                        return Err("除数为 0".into());
                    }
                    value /= divisor;
                }
                _ => return Ok(value),
            }
        }
    }

    /// factor := number | '(' expr ')'
    fn parse_factor(&mut self) -> Result<f64, String> {
        match self.next() {
            Some(Token::Num(v)) => Ok(v),
            Some(Token::LParen) => {
                let v = self.parse_expr()?;
                match self.next() {
                    Some(Token::RParen) => Ok(v),
                    _ => Err("缺少右括号".into()),
                }
            }
            Some(Token::Minus) => Ok(-self.parse_factor()?),
            _ => Err("表达式无效".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ops() {
        assert_eq!(evaluate("1+2*3").unwrap(), 7.0);
        assert_eq!(evaluate("(2+3)*4").unwrap(), 20.0);
        assert_eq!(evaluate("10/4").unwrap(), 2.5);
        assert_eq!(evaluate("-3+5").unwrap(), 2.0);
        assert_eq!(evaluate("2 * (3 + 4) / 2").unwrap(), 7.0);
    }

    #[test]
    fn errors() {
        assert!(evaluate("1/0").is_err());
        assert!(evaluate("(1+2").is_err());
        assert!(evaluate("1+").is_err());
        assert!(evaluate("foo").is_err());
    }

    #[test]
    fn operator_precedence_and_associativity() {
        assert_eq!(evaluate("2+3*4").unwrap(), 14.0);
        assert_eq!(evaluate("10-2-3").unwrap(), 5.0, "left-associative minus");
        assert_eq!(
            evaluate("100/10/2").unwrap(),
            5.0,
            "left-associative divide"
        );
        assert_eq!(evaluate("2*3+4*5").unwrap(), 26.0);
    }

    #[test]
    fn nested_parentheses() {
        assert_eq!(evaluate("((2+3)*4)").unwrap(), 20.0);
        assert_eq!(evaluate("2*(3+(4*5))").unwrap(), 46.0);
        assert_eq!(evaluate("(1+(2+(3+4)))").unwrap(), 10.0);
    }

    #[test]
    fn floating_point_math() {
        assert_eq!(evaluate("1.5*2").unwrap(), 3.0);
        assert_eq!(evaluate("0.5+0.25").unwrap(), 0.75);
        assert!((evaluate("1/3").unwrap() - (1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn unary_minus_and_parenthesized_negative() {
        assert_eq!(evaluate("-(2+3)").unwrap(), -5.0);
        assert_eq!(evaluate("-2*3").unwrap(), -6.0);
        assert_eq!(evaluate("5--3").unwrap(), 8.0);
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(evaluate("  2  +  3  ").unwrap(), 5.0);
        assert_eq!(evaluate("\t(2\n+\n3)\t").unwrap(), 5.0);
    }

    #[test]
    fn malformed_expressions_are_rejected() {
        for bad in ["1+*2", "()", "2 2", "(1+2))", "1..2", "*5", "5/", "+"] {
            assert!(evaluate(bad).is_err(), "should reject: {bad:?}");
        }
    }
}
