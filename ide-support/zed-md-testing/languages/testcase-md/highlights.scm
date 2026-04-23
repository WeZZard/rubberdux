; Storyline heading
((heading_content) @title
 (#match? @title "^Storyline$"))

; User Message heading
((heading_content) @function
 (#match? @function "^User Message$"))

; Assistant Message heading (bare or CHECK:)
((heading_content) @type
 (#match? @type "^(CHECK: )?Assistant Message$"))

; HTML comments (assertions/guidance)
(html_comment) @comment
