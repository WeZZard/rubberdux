; Storyline heading
(atx_heading
  (inline) @title
  (#match? @title "^Storyline$"))

; User Message heading
(atx_heading
  (inline) @function
  (#match? @function "^User Message$"))

; Assistant Message heading (bare or CHECK:)
(atx_heading
  (inline) @type
  (#match? @type "^(CHECK: )?Assistant Message$"))

; HTML blocks (assertions/guidance)
(html_block) @comment
