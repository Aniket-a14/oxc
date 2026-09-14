use crate::util::SemanticTester;

#[test]
fn test_class_simple() {
    SemanticTester::js(
        "
      class Foo {
        #privateProperty = 1;
        publicProperty = 2;

        constructor() {} // this method is skip
        a() {}
        set b(v) {}
        get b() {}
      }

    ",
    )
    .has_class("Foo")
    .has_number_of_elements(5)
    .has_method("a")
    .has_property("privateProperty");
}

#[test]
fn test_class_with_ts() {
    SemanticTester::ts(
        "
      class Foo {
        accessor ap = 1;
        accessor #pap = 1;
        constructor() {} // this method is skip
      }

    ",
    )
    .has_class("Foo")
    .has_number_of_elements(2)
    .has_accessor("ap")
    .has_accessor("pap");
}

#[test]
fn class_table_retains_surrogate_names() {
    let tester = SemanticTester::js(
        r#"class C { "\uD800"() {} "\ud800"() {} "\uD801"() {} "\uDC00"() {} "\uD800\uDC00"() {} }"#,
    );
    let semantic = tester.build();
    let table = semantic.classes();
    let elements = &table.elements.iter().next().unwrap().raw;
    assert_eq!(elements.len(), 5);
    assert_eq!(elements[0].name, elements[1].name);
    assert_ne!(elements[0].name, elements[2].name);
    assert_ne!(elements[0].name, elements[3].name);
    assert_eq!(elements[0].name.as_js_str().encode_utf16().collect::<Vec<_>>(), [0xD800]);
    assert_eq!(elements[4].name.as_js_str().encode_utf16().collect::<Vec<_>>(), [0xD800, 0xDC00]);
}
