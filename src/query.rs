//! Search syntax parser (ARCHITECTURE.md §7.1).
//!
//! Turns what was typed into the search box into an engine-agnostic syntax
//! tree. No Tantivy types here; `engine.rs` translates the tree.
//!
//! Words, `"phrases"` (with `*` for missing words), `-exclusions`, `OR` / `|`,
//! `AND`, `( )`, and the operators `site:`, `intitle:`, `allintitle:`,
//! `inurl:`, `allinurl:`, `intext:`, `allintext:`, `filetype:` / `ext:` and
//! `link:`. Implicit AND binds loosest, as on Google:
//! `catfish OR tilapia recipe` means `(catfish OR tilapia) AND recipe`.

use std::fmt;
use std::str::FromStr;

/// A parsed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A single word as typed (the engine tokenizes it).
    Word(String),
    /// An exact phrase.
    Phrase(String),
    /// `site:` — results from this host or its subdomains.
    Site(String),
    /// `intitle:`, `inurl:`, `intext:` (and the `allin…:` forms): the word or
    /// phrase must appear in that part of the page.
    In(Scope, Box<Node>),
    /// `filetype:` / `ext:` — the URL's file extension, lowercase, without the dot.
    Ext(String),
    /// `link:` — pages linking to this URL, or to anywhere on this site.
    Link(String),
    /// Results must not match this.
    Not(Box<Node>),
    /// All of these must match.
    And(Vec<Node>),
    /// At least one of these must match.
    Or(Vec<Node>),
}

/// The part of a page an `in…:` operator looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Title,
    Url,
    Text,
}

/// `cache:URL` (Google's operator for the stored copy of a page): the URL,
/// if that's what `input` asks for. It only works on its own, so `cache:`
/// with other words, or with nothing after it, is an error.
pub fn cache_url(input: &str) -> Option<std::result::Result<String, String>> {
    let input = input.trim();
    if !input.get(..6)?.eq_ignore_ascii_case("cache:") {
        return None;
    }
    let url = input[6..].trim();
    Some(if url.is_empty() || url.contains(char::is_whitespace) {
        Err("cache: takes one address on its own, e.g. cache:https://example.com/page".into())
    } else {
        Ok(url.to_string())
    })
}

/// Parses a query. Returns `None` if it contains nothing searchable.
pub fn parse(input: &str) -> Option<Node> {
    let tokens = tokenize(input);
    let mut parser = Parser {
        tokens,
        pos: 0,
        all: None,
    };
    parser.parse_and(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Site,
    In(Scope),
    AllIn(Scope),
    Ext,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Phrase(String),
    /// An operator; its value is `None` when it's written apart from it
    /// (`intitle: catfish`) or followed by a phrase (`intitle:"fried catfish"`).
    Op(Op, Option<String>),
    Neg,
    Or,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();

    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        match c {
            '(' => {
                tokens.push(Token::LParen);
                chars.next();
            }
            ')' => {
                tokens.push(Token::RParen);
                chars.next();
            }
            '|' => {
                tokens.push(Token::Or);
                chars.next();
            }
            '"' => {
                chars.next();
                let start = i + 1;
                let mut end = input.len();
                for (j, c) in chars.by_ref() {
                    if c == '"' {
                        end = j;
                        break;
                    }
                }
                let phrase = input[start..end].trim();
                if !phrase.is_empty() {
                    tokens.push(Token::Phrase(phrase.to_string()));
                }
            }
            '-' => {
                chars.next();
                // Only a negation when glued to what follows ("-yandex", "-\"a b\"").
                match chars.peek() {
                    Some(&(_, next)) if !next.is_whitespace() && next != ')' && next != '-' => {
                        tokens.push(Token::Neg)
                    }
                    _ => {}
                }
            }
            _ => {
                let start = i;
                let mut end = input.len();
                while let Some(&(j, c)) = chars.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' || c == '"' || c == '|' {
                        end = j;
                        break;
                    }
                    chars.next();
                }
                let word = &input[start..end];
                tokens.push(classify(word));
            }
        }
    }
    tokens
}

fn classify(word: &str) -> Token {
    if word == "OR" {
        return Token::Or;
    }
    if let Some((name, value)) = word.split_once(':') {
        let op = match name.to_ascii_lowercase().as_str() {
            "site" => Some(Op::Site),
            "intitle" => Some(Op::In(Scope::Title)),
            "inurl" => Some(Op::In(Scope::Url)),
            "intext" => Some(Op::In(Scope::Text)),
            "allintitle" => Some(Op::AllIn(Scope::Title)),
            "allinurl" => Some(Op::AllIn(Scope::Url)),
            "allintext" => Some(Op::AllIn(Scope::Text)),
            "filetype" | "ext" => Some(Op::Ext),
            "link" => Some(Op::Link),
            _ => None,
        };
        if let Some(op) = op {
            let value = (!value.is_empty()).then(|| value.to_string());
            return Token::Op(op, value);
        }
    }
    Token::Word(word.to_string())
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Set by `allintitle:` and friends: every later word goes there.
    all: Option<Scope>,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    /// Implicit AND of everything until the end (or a closing parenthesis).
    fn parse_and(&mut self, nested: bool) -> Option<Node> {
        let mut nodes = Vec::new();
        while let Some(token) = self.peek() {
            match token {
                Token::RParen if nested => break,
                Token::RParen | Token::Or => {
                    // Stray: ignore.
                    self.pos += 1;
                }
                Token::Word(w) if w == "AND" => {
                    self.pos += 1;
                }
                _ => {
                    if let Some(node) = self.parse_or() {
                        nodes.push(node);
                    }
                }
            }
        }
        match nodes.len() {
            0 => None,
            1 => nodes.pop(),
            _ => Some(Node::And(nodes)),
        }
    }

    fn parse_or(&mut self) -> Option<Node> {
        let first = self.parse_unary();
        let mut alternatives: Vec<Node> = first.into_iter().collect();
        while self.peek() == Some(&Token::Or) {
            self.pos += 1;
            if let Some(node) = self.parse_unary() {
                alternatives.push(node);
            }
        }
        match alternatives.len() {
            0 => None,
            1 => alternatives.pop(),
            _ => Some(Node::Or(alternatives)),
        }
    }

    fn parse_unary(&mut self) -> Option<Node> {
        match self.peek()? {
            Token::Neg => {
                self.pos += 1;
                self.parse_unary().map(|n| Node::Not(Box::new(n)))
            }
            Token::Or | Token::RParen => None,
            _ => self.parse_atom(),
        }
    }

    fn parse_atom(&mut self) -> Option<Node> {
        match self.next()? {
            Token::LParen => {
                let inner = self.parse_and(true);
                if self.peek() == Some(&Token::RParen) {
                    self.pos += 1;
                }
                inner
            }
            Token::Word(w) => Some(self.scoped(Node::Word(w))),
            Token::Phrase(p) => Some(self.scoped(Node::Phrase(p))),
            Token::Op(op, value) => self.operator(op, value),
            Token::Neg | Token::Or | Token::RParen => None,
        }
    }

    /// Applies an `allin…:` scope, if one is active.
    fn scoped(&self, node: Node) -> Node {
        match self.all {
            Some(scope) => Node::In(scope, Box::new(node)),
            None => node,
        }
    }

    /// The value written after an operator, or else the word or phrase right after it.
    fn operand(&mut self, value: Option<String>) -> Option<Node> {
        if let Some(v) = value {
            return Some(Node::Word(v));
        }
        match self.peek() {
            Some(Token::Word(_)) | Some(Token::Phrase(_)) => match self.next()? {
                Token::Word(w) => Some(Node::Word(w)),
                Token::Phrase(p) => Some(Node::Phrase(p)),
                _ => None,
            },
            _ => None,
        }
    }

    fn operator(&mut self, op: Op, value: Option<String>) -> Option<Node> {
        let text = |node: Node| match node {
            Node::Word(w) | Node::Phrase(w) => w,
            _ => String::new(),
        };
        match op {
            Op::Site => self.operand(value).map(|n| Node::Site(text(n))),
            Op::In(scope) => self.operand(value).map(|n| Node::In(scope, Box::new(n))),
            Op::AllIn(scope) => {
                self.all = Some(scope);
                value.map(|v| Node::In(scope, Box::new(Node::Word(v))))
            }
            Op::Ext => self
                .operand(value)
                .map(|n| Node::Ext(text(n).trim_start_matches('.').to_ascii_lowercase())),
            Op::Link => self.operand(value).map(|n| Node::Link(text(n))),
        }
    }
}

/// The words a result is expected to contain: every word and phrase that
/// isn't excluded. Used to highlight matches.
pub fn positive_terms(node: &Node) -> Vec<String> {
    fn walk(node: &Node, out: &mut Vec<String>) {
        match node {
            Node::Word(w) | Node::Phrase(w) => out.push(w.clone()),
            Node::And(children) | Node::Or(children) => {
                children.iter().for_each(|c| walk(c, out));
            }
            Node::In(_, inner) => walk(inner, out),
            Node::Not(_) | Node::Site(_) | Node::Ext(_) | Node::Link(_) => {}
        }
    }
    let mut out = Vec::new();
    walk(node, &mut out);
    out
}

/// Whether a query asks for anything at all: something other than exclusions.
/// `-site:example.com` alone finds nothing, as on Google.
pub fn has_positive(node: &Node) -> bool {
    match node {
        Node::Not(_) => false,
        Node::And(children) | Node::Or(children) => children.iter().any(has_positive),
        Node::In(_, inner) => has_positive(inner),
        _ => true,
    }
}

/// Splits a phrase at standalone `*` words: the text chunks in order, each
/// with the number of `*` right before it. Leading and trailing `*` say
/// nothing and are dropped: `"how to * catfish"` gives `[(0, "how to"), (1, "catfish")]`.
pub fn wildcard_chunks(phrase: &str) -> Vec<(usize, String)> {
    let mut chunks: Vec<(usize, String)> = Vec::new();
    let mut stars = 0;
    for word in phrase.split_whitespace() {
        if word.chars().all(|c| c == '*') {
            stars += word.len();
            continue;
        }
        match chunks.last_mut() {
            Some((_, text)) if stars == 0 => {
                text.push(' ');
                text.push_str(word);
            }
            Some(_) => chunks.push((stars, word.to_string())),
            None => chunks.push((0, word.to_string())),
        }
        stars = 0;
    }
    chunks
}

/// Every way to size the gaps between phrase chunks, as on Google: `n` stars
/// in a row stand for `n` to `n + 2` words. If that gives more than `max`
/// combinations, each gap gets its smallest size only.
pub fn gap_layouts(stars: &[usize], max: usize) -> Vec<Vec<usize>> {
    let total: usize = stars.iter().map(|_| 3usize).product();
    if stars.is_empty() || total > max {
        return vec![stars.to_vec()];
    }
    let mut layouts = vec![Vec::new()];
    for &n in stars {
        layouts = layouts
            .into_iter()
            .flat_map(|layout: Vec<usize>| {
                (n..=n + 2).map(move |size| {
                    let mut next = layout.clone();
                    next.push(size);
                    next
                })
            })
            .collect();
    }
    layouts
}

/// `[OFFSET,]COUNT`, with SQL `LIMIT` semantics (ARCHITECTURE.md §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub offset: usize,
    pub count: usize,
}

impl Limit {
    pub fn new(offset: usize, count: usize) -> Limit {
        Limit { offset, count }
    }
}

impl FromStr for Limit {
    type Err = String;

    fn from_str(s: &str) -> Result<Limit, String> {
        let number = |part: &str| {
            part.trim()
                .parse::<usize>()
                .map_err(|_| format!("invalid limit \"{s}\": expected COUNT or OFFSET,COUNT"))
        };
        match s.split_once(',') {
            Some((offset, count)) => Ok(Limit::new(number(offset)?, number(count)?)),
            None => Ok(Limit::new(0, number(s)?)),
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{}", self.offset, self.count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_lookups() {
        assert_eq!(cache_url("catfish"), None);
        assert_eq!(cache_url("cache"), None);
        assert_eq!(
            cache_url(" CACHE:https://example.com/a "),
            Some(Ok("https://example.com/a".into()))
        );
        assert_eq!(
            cache_url("cache: example.com"),
            Some(Ok("example.com".into()))
        );
        assert!(matches!(cache_url("cache:"), Some(Err(_))));
        assert!(matches!(
            cache_url("cache:example.com catfish"),
            Some(Err(_))
        ));
    }

    fn w(s: &str) -> Node {
        Node::Word(s.into())
    }

    #[test]
    fn words_and_phrases() {
        assert_eq!(parse("catfish"), Some(w("catfish")));
        assert_eq!(
            parse("catfish  recipe"),
            Some(Node::And(vec![w("catfish"), w("recipe")]))
        );
        assert_eq!(
            parse("\"fried catfish\" recipe"),
            Some(Node::And(vec![
                Node::Phrase("fried catfish".into()),
                w("recipe")
            ]))
        );
        assert_eq!(
            parse("\"unterminated phrase"),
            Some(Node::Phrase("unterminated phrase".into()))
        );
        assert_eq!(parse("   "), None);
        assert_eq!(parse("\"\""), None);
    }

    #[test]
    fn exclusions() {
        assert_eq!(
            parse("Russian search engine -yandex"),
            Some(Node::And(vec![
                w("Russian"),
                w("search"),
                w("engine"),
                Node::Not(Box::new(w("yandex")))
            ]))
        );
        assert_eq!(
            parse("yandex -site:yandex.com"),
            Some(Node::And(vec![
                w("yandex"),
                Node::Not(Box::new(Node::Site("yandex.com".into())))
            ]))
        );
        assert_eq!(parse("e-mail"), Some(w("e-mail")));
        assert_eq!(parse("a - b"), Some(Node::And(vec![w("a"), w("b")])));
        assert_eq!(
            parse("-\"exact phrase\""),
            Some(Node::Not(Box::new(Node::Phrase("exact phrase".into()))))
        );
    }

    #[test]
    fn or_binds_tighter_than_and() {
        let expected = Some(Node::And(vec![
            Node::Or(vec![w("catfish"), w("tilapia")]),
            w("recipe"),
        ]));
        assert_eq!(parse("catfish OR tilapia recipe"), expected);
        assert_eq!(parse("catfish | tilapia recipe"), expected);
        assert_eq!(parse("catfish|tilapia recipe"), expected);
        assert_eq!(parse("(catfish OR tilapia) AND recipe"), expected);
        // Lowercase "or" is just a word.
        assert_eq!(
            parse("this or that"),
            Some(Node::And(vec![w("this"), w("or"), w("that")]))
        );
    }

    #[test]
    fn grouping_and_stray_tokens() {
        assert_eq!(
            parse("(a b) OR c"),
            Some(Node::Or(vec![Node::And(vec![w("a"), w("b")]), w("c")]))
        );
        assert_eq!(parse("OR a"), Some(w("a")));
        assert_eq!(parse("a OR"), Some(w("a")));
        assert_eq!(parse("a )"), Some(w("a")));
        assert_eq!(parse("(a"), Some(w("a")));
        assert_eq!(parse("site:"), None);
        assert_eq!(
            parse("SITE:example.com"),
            Some(Node::Site("example.com".into()))
        );
    }

    fn within(scope: Scope, node: Node) -> Node {
        Node::In(scope, Box::new(node))
    }

    #[test]
    fn scope_operators() {
        assert_eq!(
            parse("intitle:catfish recipe"),
            Some(Node::And(vec![
                within(Scope::Title, w("catfish")),
                w("recipe")
            ]))
        );
        assert_eq!(
            parse("intitle:\"fried catfish\""),
            Some(within(Scope::Title, Node::Phrase("fried catfish".into())))
        );
        assert_eq!(parse("inurl:blog"), Some(within(Scope::Url, w("blog"))));
        assert_eq!(
            parse("INTEXT:cornmeal"),
            Some(within(Scope::Text, w("cornmeal")))
        );
        assert_eq!(
            parse("-intitle:spam ham"),
            Some(Node::And(vec![
                Node::Not(Box::new(within(Scope::Title, w("spam")))),
                w("ham")
            ]))
        );
        assert_eq!(
            parse("intitle: catfish"),
            Some(within(Scope::Title, w("catfish")))
        );
        assert_eq!(parse("intitle:"), None);
    }

    #[test]
    fn all_in_operators_cover_the_rest_of_the_query() {
        assert_eq!(
            parse("allintitle: fried catfish"),
            Some(Node::And(vec![
                within(Scope::Title, w("fried")),
                within(Scope::Title, w("catfish"))
            ]))
        );
        assert_eq!(
            parse("allinurl:blog \"rust lang\""),
            Some(Node::And(vec![
                within(Scope::Url, w("blog")),
                within(Scope::Url, Node::Phrase("rust lang".into()))
            ]))
        );
    }

    #[test]
    fn file_types_and_links() {
        assert_eq!(parse("filetype:PDF"), Some(Node::Ext("pdf".into())));
        assert_eq!(parse("ext:.pdf"), Some(Node::Ext("pdf".into())));
        assert_eq!(
            parse("link:example.com"),
            Some(Node::Link("example.com".into()))
        );
        assert_eq!(
            parse("link:https://example.com/a"),
            Some(Node::Link("https://example.com/a".into()))
        );
        assert_eq!(parse("https://example.com"), Some(w("https://example.com")));
        assert_eq!(parse("foo:bar"), Some(w("foo:bar")));
    }

    #[test]
    fn exclusions_alone_ask_for_nothing() {
        assert!(!has_positive(&parse("-site:news.ycombinator.com").unwrap()));
        assert!(!has_positive(&parse("-a -b").unwrap()));
        assert!(!has_positive(&parse("(-a)").unwrap()));
        assert!(has_positive(&parse("a -b").unwrap()));
        assert!(has_positive(&parse("site:x.com").unwrap()));
    }

    #[test]
    fn wildcards_in_phrases() {
        assert_eq!(
            wildcard_chunks("how to * catfish"),
            vec![(0, "how to".to_string()), (1, "catfish".to_string())]
        );
        assert_eq!(
            wildcard_chunks("* a ** b c *"),
            vec![(0, "a".to_string()), (2, "b c".to_string())]
        );
        assert_eq!(
            wildcard_chunks("no stars"),
            vec![(0, "no stars".to_string())]
        );
        assert_eq!(gap_layouts(&[1], 9), vec![vec![1], vec![2], vec![3]]);
        assert_eq!(gap_layouts(&[1, 1], 9).len(), 9);
        assert_eq!(gap_layouts(&[1, 1, 1], 9), vec![vec![1, 1, 1]]);
        assert_eq!(gap_layouts(&[], 9), vec![Vec::<usize>::new()]);
    }

    #[test]
    fn terms_to_highlight() {
        let node = parse(
            "(catfish OR \"fried fish\") recipe intitle:easy -yandex -site:x.com filetype:pdf",
        )
        .unwrap();
        assert_eq!(
            positive_terms(&node),
            vec!["catfish", "fried fish", "recipe", "easy"]
        );
    }

    #[test]
    fn limits() {
        assert_eq!("10".parse(), Ok(Limit::new(0, 10)));
        assert_eq!("10,100".parse(), Ok(Limit::new(10, 100)));
        assert_eq!(" 5 , 7 ".parse(), Ok(Limit::new(5, 7)));
        assert!("x".parse::<Limit>().is_err());
        assert!("1,".parse::<Limit>().is_err());
        assert!("-1".parse::<Limit>().is_err());
    }
}
