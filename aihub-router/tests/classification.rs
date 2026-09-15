use aihub_core::TaskTier;
use aihub_router::classify;

#[test]
fn recognizes_a_portuguese_mechanical_request() {
    let answer = classify("refatora essa função");
    assert_eq!(answer.tier, TaskTier::Mechanical);
    assert!(!answer.ambiguous);
}

#[test]
fn bilingual_prompt_battery() {
    use TaskTier::*;
    let groups = [
        (
            Mechanical,
            vec![
                "refactor this function",
                "rename the variable",
                "fix typo in README",
                "add unit test for parsing",
                "lint this module",
                "format the file",
                "add tests for edge cases",
                "fix this error",
                "refatora essa função",
                "renomeia a variável",
                "corrige o erro",
                "adiciona teste de regressão",
                "adicione testes de unidade",
                "formata o arquivo",
                "refatorar o módulo",
                "renomeie esse método",
            ],
        ),
        (
            Review,
            vec![
                "check diff",
                "audit authentication",
                "security review",
                "explain this function",
                "find bug in parser",
                "find the bug",
                "revisa esse código",
                "audita o login",
                "explica essa função",
                "acha o bug",
                "revise as mudanças",
                "encontre o bug",
                "explique o fluxo",
                "revisão de segurança",
            ],
        ),
        (
            Design,
            vec![
                "architect a storage system",
                "design the queue",
                "plan a migration",
                "write an RFC",
                "architecture for messaging",
                "arquitetura do serviço",
                "desenha a solução",
                "planeja a migração",
                "planeje o sistema",
                "desenhe o fluxo",
                "planejamento da API",
                "RFC para o cache",
            ],
        ),
    ];
    for (tier, prompts) in groups {
        for prompt in prompts {
            let result = classify(prompt);
            assert_eq!(result.tier, tier, "{prompt}");
            assert!(!result.ambiguous, "{prompt}");
        }
    }
}

#[test]
fn uncertainty_and_boundaries_are_explicit() {
    for prompt in [
        "",
        "hello",
        "planet designer explanation",
        "faz isso funcionar",
    ] {
        let result = classify(prompt);
        assert!(result.ambiguous, "{prompt}");
        assert_eq!(result.tier, TaskTier::Design);
    }
    for (prompt, tier) in [
        ("refatora e revisa", TaskTier::Review),
        ("audit and design", TaskTier::Design),
        ("planeja e corrige", TaskTier::Design),
    ] {
        let result = classify(prompt);
        assert!(result.ambiguous);
        assert_eq!(result.tier, tier);
    }
    assert_eq!(classify("REVISE: segurança!").tier, TaskTier::Review);
    assert!(classify(&format!("refatora {}", "é".repeat(5000))).ambiguous);
}

#[test]
fn heuristic_latency_budget() {
    let prompt = "Por favor refatora essa função e adiciona teste de regressão.";
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        std::hint::black_box(classify(std::hint::black_box(prompt)));
    }
    let average = start.elapsed() / 1000;
    eprintln!("classification average: {average:?}");
    assert!(average < std::time::Duration::from_millis(2));
}
