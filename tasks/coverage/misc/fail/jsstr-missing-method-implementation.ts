class MissingLead {
  "\uD800"(): void;
  "\uD801"() {}
}
class MissingTrail {
  "\uDC00"(): void;
  "\uDC01"() {}
}
class MissingEnd {
  "before\uD800after"(): void;
}
