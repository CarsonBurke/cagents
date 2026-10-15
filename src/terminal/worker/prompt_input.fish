# Render the original prompt before printing markers: even a harmless printf
# would replace $pipestatus before themed prompts can inspect it. Capturing
# with string collect preserves embedded and trailing newlines exactly.
functions --copy fish_prompt __cagents_original_prompt
function fish_prompt
    set -l rendered (__cagents_original_prompt | string collect --allow-empty --no-trim-newlines)
    set -l prompt_status $pipestatus[1]
    printf '\e]133;A\a%s\e]133;B\a' "$rendered"
    return $prompt_status
end
if functions --query fish_right_prompt
    functions --copy fish_right_prompt __cagents_original_right_prompt
    function fish_right_prompt
        set -l rendered (__cagents_original_right_prompt | string collect --allow-empty --no-trim-newlines)
        set -l prompt_status $pipestatus[1]
        printf '\e]133;P;k=r\a%s\e]133;B\a' "$rendered"
        return $prompt_status
    end
end
function __cagents_preexec --on-event fish_preexec
    printf '\e]133;C\a'
end
function __cagents_postexec --on-event fish_postexec
    printf '\e]133;D;%s\a' $status
end
