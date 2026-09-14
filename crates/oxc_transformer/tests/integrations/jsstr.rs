use rustc_hash::FxHashSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{ClassElement, PropertyKey, Statement};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::TransformOptions;

#[test]
fn legacy_accessor_storage_preserves_jsstr_keys() {
    let source = r#"class C {
        #_ref_accessor_storage;
        accessor "\uD800" = 1;
        accessor "\uD801" = 2;
        accessor "\uDC00" = 3;
        accessor "a-b" = 4;
        accessor "a\uD800b" = 5;
    }"#;
    let mut options = TransformOptions::default();
    options.decorator.legacy = true;
    let code = super::test_with_source_type(source, SourceType::ts(), &options).unwrap();
    let allocator = Allocator::new();
    let parsed = Parser::new(&allocator, &code, SourceType::mjs()).parse();
    assert!(parsed.diagnostics.is_empty(), "{code}\n{:?}", parsed.diagnostics);
    let semantic = SemanticBuilder::new().with_check_syntax_error(true).build(&parsed.program);
    assert!(semantic.diagnostics.is_empty(), "{code}\n{:?}", semantic.diagnostics);
    let Statement::ClassDeclaration(class) = &parsed.program.body[0] else { panic!("{code}") };
    let mut storage = FxHashSet::default();
    let mut getters = Vec::new();
    let mut setters = Vec::new();
    for element in &class.body.body {
        match element {
            ClassElement::PropertyDefinition(property) => {
                let PropertyKey::PrivateIdentifier(name) = &property.key else { panic!("{code}") };
                assert!(storage.insert(name.name));
            }
            ClassElement::MethodDefinition(method) => {
                let name = method
                    .key
                    .static_name()
                    .unwrap()
                    .as_js_str()
                    .encode_utf16()
                    .collect::<Vec<_>>();
                if method.kind.is_get() {
                    getters.push(name);
                } else {
                    setters.push(name);
                }
            }
            _ => panic!("{code}"),
        }
    }
    assert_eq!(storage.len(), 6);
    assert_eq!(getters, setters);
    assert_eq!(
        getters,
        [vec![0xD800], vec![0xD801], vec![0xDC00], vec![97, 45, 98], vec![97, 0xD800, 98]]
    );
}

#[test]
fn jsx_attribute_entities_and_existing_surrogates_round_trip() {
    use oxc_ast::ast::{Expression, JSXAttributeItem, JSXAttributeValue, ObjectProperty};
    use oxc_ast_visit::{Visit, walk};
    use oxc_codegen::Codegen;
    use oxc_str::JSStrBuilder;
    use oxc_transformer::Transformer;

    #[derive(Default)]
    struct Values(Vec<Vec<u16>>);
    impl<'a> Visit<'a> for Values {
        fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
            if property.key.is_specific_static_name("value")
                && let Expression::StringLiteral(value) = &property.value
            {
                self.0.push(value.value.encode_utf16().collect());
            }
            walk::walk_object_property(self, property);
        }
    }

    for unit in [0xD800, 0xD801, 0xDC00] {
        for reparse in [false, true] {
            let allocator = Allocator::new();
            let mut parsed =
                Parser::new(&allocator, r#"<X value="" />;"#, SourceType::jsx()).parse();
            assert!(parsed.diagnostics.is_empty());
            let Statement::ExpressionStatement(statement) = &mut parsed.program.body[0] else {
                panic!()
            };
            let Expression::JSXElement(element) = &mut statement.expression else { panic!() };
            let JSXAttributeItem::Attribute(attribute) = &mut element.opening_element.attributes[0]
            else {
                panic!()
            };
            let Some(JSXAttributeValue::StringLiteral(literal)) = &mut attribute.value else {
                panic!()
            };
            let mut value = JSStrBuilder::new_in(&allocator);
            value.push_str("π&amp;\"'");
            value.push_code_unit(unit);
            value.push_str("&quot;😀");
            literal.value = value.into_js_str();
            literal.raw = None;
            let code = Codegen::new().build(&parsed.program).code;
            if reparse {
                parsed = Parser::new(&allocator, &code, SourceType::jsx()).parse();
                assert!(parsed.diagnostics.is_empty(), "{code}");
            }
            let mut program = parsed.program;
            let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
            let options = TransformOptions::default();
            let transformed =
                Transformer::new(&allocator, std::path::Path::new("test.jsx"), &options)
                    .build_with_scoping(scoping, &mut program);
            assert!(transformed.diagnostics.is_empty(), "{:?}", transformed.diagnostics);
            let output = Codegen::new().build(&program).code;
            let reparsed = Parser::new(&allocator, &output, SourceType::mjs()).parse();
            assert!(reparsed.diagnostics.is_empty(), "{output}");
            let mut values = Values::default();
            values.visit_program(&reparsed.program);
            let expected: Vec<_> =
                "π&\"'".encode_utf16().chain([unit]).chain("\"😀".encode_utf16()).collect();
            assert_eq!(values.0, [expected], "{output}");
        }
    }
}
