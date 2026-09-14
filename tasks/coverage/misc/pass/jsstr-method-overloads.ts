class Overloads {
  "\uD800"(value: number): void;
  "\ud800"(value: unknown) {}
  "\uDC00"(value: number): void;
  "\udc00"(value: unknown) {}
  "\uD800\uDC00"(value: number): void;
  "𐀀"(value: unknown) {}
  "before\uD801after"(value: number): void;
  "before\ud801after"(value: unknown) {}
}
